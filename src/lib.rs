use anyhow::{anyhow, ensure, Context, Result};
use crossbeam::thread;
use crossterm::{cursor, queue, terminal};
use serde::Deserialize;
use solana_client::{
    rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    bpf_loader_upgradeable::{
        close, create_buffer, deploy_with_max_program_len, upgrade, write,
        UpgradeableLoaderState,
    },
    commitment_config::{CommitmentConfig, CommitmentLevel},
    hash::Hash,
    message::Message,
    native_token::lamports_to_sol,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
    transaction::Transaction,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

mod utils;
pub use utils::*;

#[derive(Deserialize)]
pub struct Config {
    // TODO: monikers
    pub url: String,
    // TODO: search in target/deploy ?
    pub program_so: PathBuf,
    pub options: Options,
    pub keypairs: Keypairs,
}

#[derive(Deserialize)]
pub struct Options {
    #[serde(default = "num_cpus::get")]
    pub jobs: usize,
    pub max_retries: Option<usize>,
    pub sleep: u64,
    pub timeout: u64,
}

#[derive(Deserialize)]
pub struct Keypairs {
    // TODO: use solana_cli default
    pub authority: PathBuf,
    pub program: Option<PathBuf>,
}

pub struct AppConfig {
    pub url: String,
    pub program_data: Vec<u8>,
    pub program_keypair: Keypair,
    pub authority: Keypair,
    pub send_config: RpcSendTransactionConfig,
    pub client: RpcClient,
    pub options: Options,
}

impl AppConfig {
    pub fn parse<P: AsRef<Path>>(p: P) -> Result<Self> {
        let toml_config =
            std::fs::read(p).context("Failed to read config file.")?;
        let config: Config = toml::from_slice(&toml_config)
            .context("Failed to parse config file.")?;

        let authority = read_keypair_file(&config.keypairs.authority)
            .map_err(|e| anyhow!("Couldn't read payer keypair: {e}"))?;

        let program_kp_path = match config.keypairs.program {
            Some(kp) => kp,
            None => Self::search_program_kp()?,
        };
        let program_keypair = read_keypair_file(program_kp_path)
            .map_err(|err| anyhow!("Couldn't read program keypair: {}", err))?;
        let program_data = read_and_verify_elf(&config.program_so)?;

        let send_config = RpcSendTransactionConfig {
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            max_retries: config.options.max_retries,
            ..Default::default()
        };

        let client = RpcClient::new_with_timeouts_and_commitment(
            &config.url,
            Duration::from_secs(config.options.timeout),
            CommitmentConfig::confirmed(),
            Duration::from_secs(5),
        );

        Ok(Self {
            options: config.options,
            url: config.url,
            send_config,
            client,
            authority,
            program_data,
            program_keypair,
        })
    }

    pub fn search_program_kp() -> Result<PathBuf> {
        // Look for "*-keypair.json" at ./target/deploy
        let keypair_file = std::fs::read_dir("./target/deploy")
            .context(
                "Error looking for program keypair file at ./target/deploy",
            )?
            .find(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .into_string()
                    .unwrap()
                    .contains("-keypair.json")
            })
            .transpose()?
            .ok_or_else(|| {
                anyhow!("No keypair file found in ./target/deploy")
            })?;

        println!(
            "Using {} as program keypair.",
            keypair_file.path().to_string_lossy()
        );

        Ok(keypair_file.path())
    }

    pub fn search_payer_kp() -> PathBuf {
        unimplemented!()
    }
}

pub fn create_buffer_account(config: &AppConfig) -> Result<(Keypair, usize)> {
    let buffer_kp = Keypair::new();
    let buffer_sz =
        UpgradeableLoaderState::buffer_len(config.program_data.len())?;
    let min_balance = config
        .client
        .get_minimum_balance_for_rent_exemption(buffer_sz)?;
    let payer_balance =
        config.client.get_balance(&config.authority.pubkey())?;

    println!(
        "Need {} SOL to create buffer account. Current balance is: {}",
        lamports_to_sol(min_balance),
        lamports_to_sol(payer_balance),
    );
    ensure!(payer_balance >= min_balance, "Insufficient funds.");

    let ix = create_buffer(
        &config.authority.pubkey(),
        &buffer_kp.pubkey(),
        &config.authority.pubkey(),
        min_balance,
        config.program_data.len(),
    )?;

    let blockhash = config.client.get_latest_blockhash()?;

    let tx = Transaction::new_signed_with_payer(
        &ix,
        Some(&config.authority.pubkey()),
        &[&config.authority, &buffer_kp],
        blockhash,
    );
    config
        .client
        .send_and_confirm_transaction_with_spinner_and_config(
            &tx,
            CommitmentConfig::confirmed(),
            config.send_config,
        )
        .context("Create buffer tx error")?;

    Ok((buffer_kp, buffer_sz))
}

pub fn write_to_buffer_account(
    config: &AppConfig,
    buffer_acc: Pubkey,
    buffer_len: usize,
) -> Result<()> {
    let payer = &config.authority;
    let client = &config.client;
    let program_data = &config.program_data;
    let jobs = config.options.jobs;

    let create_msg = |offset: u32, bytes: Vec<u8>, blockhash: Hash| {
        Message::new_with_blockhash(
            &[write(&buffer_acc, &payer.pubkey(), offset, bytes)],
            Some(&payer.pubkey()),
            &blockhash,
        )
    };

    let chunk_sz = calculate_max_chunk_size(&create_msg)?;
    let tx_count = buffer_len / chunk_sz + 2;
    let mut blockhash = client.get_latest_blockhash()?;
    let mut start_time = Instant::now();

    for (i, chunks) in program_data.chunks(chunk_sz * jobs).enumerate() {
        if start_time.elapsed().as_secs() > 30 {
            start_time = Instant::now();
            blockhash = client
                .get_latest_blockhash()
                .context("Couldn't get recent blockhash")?
        };

        let result = thread::scope(move |s| {
            for j in 0..config.options.jobs {
                let mut stdout = std::io::stdout();
                let total_index = i * config.options.jobs + j;

                s.spawn(move |_| -> Result<()> {
                    let offset = (total_index * chunk_sz) as u32;

                    if offset >= program_data.len() as u32 {
                        return Ok(());
                    }

                    let bytes = chunks
                        .chunks(chunk_sz)
                        .nth(j)
                        .ok_or_else(|| anyhow!("Failed to read thread chunk"))?
                        .to_vec();
                    let msg = create_msg(offset, bytes, blockhash);
                    let tx = Transaction::new(&[payer], msg, blockhash);
                    let tx_sig = send_and_confirm_transaction_with_config(
                        client,
                        &tx,
                        client.commitment(),
                        config.send_config,
                        config.options.timeout,
                        config.options.sleep,
                    )
                    .context("Write tx error.")?;

                    queue!(stdout, cursor::SavePosition)?;
                    stdout.write_all(
                        format!(
                            "Confirmed ({}/{}): {}",
                            total_index + 2,
                            tx_count,
                            tx_sig
                        )
                        .as_ref(),
                    )?;
                    queue!(stdout, cursor::RestorePosition)?;
                    stdout.flush()?;
                    queue!(
                        stdout,
                        cursor::RestorePosition,
                        terminal::Clear(terminal::ClearType::FromCursorDown)
                    )?;

                    Ok(())
                });
            }
        });

        if result.is_err() {
            close_buffer_account(config, buffer_acc)?;
        }
    }

    Ok(())
}

pub fn deploy_or_upgrade_program(
    config: &AppConfig,
    buffer_acc: Pubkey,
) -> Result<()> {
    let client = &config.client;
    let program = &config.program_keypair;
    let payer = &config.authority;

    let program_acc = client.get_account(&program.pubkey());
    let blockhash = client
        .get_latest_blockhash()
        .context("Couldn't get recent blockhash.")?;

    let tx = match program_acc {
        Err(_) => {
            println!("Deploying {}", program.pubkey());

            let program_lamports = client
                .get_minimum_balance_for_rent_exemption(
                    UpgradeableLoaderState::program_len()?,
                )
                .context("Couldn't get balance for program.")?;

            let ixs = deploy_with_max_program_len(
                &payer.pubkey(),
                &program.pubkey(),
                &buffer_acc,
                &payer.pubkey(),
                program_lamports,
                config.program_data.len() * 2,
            )?;

            Transaction::new_signed_with_payer(
                &ixs,
                Some(&payer.pubkey()),
                &[payer, program],
                blockhash,
            )
        }
        Ok(_) => {
            println!("Upgrading {}", program.pubkey());
            Transaction::new_signed_with_payer(
                &[upgrade(
                    &program.pubkey(),
                    &buffer_acc,
                    &payer.pubkey(),
                    &payer.pubkey(),
                )],
                Some(&payer.pubkey()),
                &[payer],
                blockhash,
            )
        }
    };

    client.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        client.commitment(),
        config.send_config,
    )?;

    Ok(())
}

pub fn close_buffer_account(
    config: &AppConfig,
    buffer_acc: Pubkey,
) -> Result<()> {
    let client = &config.client;
    let payer = &config.authority;
    let blockhash = client
        .get_latest_blockhash()
        .context("Failed to fetch latest blockhash.")?;
    let close_ix = close(&buffer_acc, &payer.pubkey(), &payer.pubkey());
    let close_tx = Transaction::new_signed_with_payer(
        &[close_ix],
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );

    client
        .send_and_confirm_transaction_with_spinner_and_config(
            &close_tx,
            client.commitment(),
            config.send_config,
        )
        .context("Unable to close the buffer")?;

    Ok(())
}
