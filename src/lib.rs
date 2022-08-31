use anyhow::{anyhow, ensure, Context, Result};
use crossbeam::thread;
use crossterm::{cursor, queue, terminal};
use serde::{Deserialize, Serialize};
use solana_client::{
    client_error::reqwest::Url, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    bpf_loader_upgradeable::{
        close, create_buffer, deploy_with_max_program_len, upgrade, write,
        UpgradeableLoaderState,
    },
    commitment_config::{CommitmentConfig, CommitmentLevel},
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

#[derive(Serialize, Deserialize)]
struct Config {
    // TODO: monikers
    pub url: String,
    pub program: Program,
    pub options: Options,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Program {
    // TODO: use solana_cli default
    pub authority: PathBuf,
    pub keypair: PathBuf,
    // TODO: search in target/deploy ?
    pub shared_obj: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Options {
    pub jobs: usize,
    pub max_retries: Option<usize>,
    pub sleep: u64,
    pub timeout: u64,
}

/// Generates a new configuration file using the defaults and tries to find the program keypair and
/// shared object files in ./target/deploy.
pub fn generate_config<W: Write>(writer: &mut W, cwd: &Path) -> Result<()> {
    let deploy_dir = cwd.join("target").join("deploy");
    let program = match std::fs::read_dir(deploy_dir) {
        Err(_) => Program::default(),
        // Try to find program-keypair.json and program.so
        Ok(entries) => entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .into_string()
                    .unwrap_or_default()
                    .ends_with("-keypair.json")
                    || e.path()
                        .extension()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .eq("so")
            })
            .fold(Program::default(), |mut acc, curr| {
                let field = if curr
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .ends_with("-keypair.json")
                {
                    &mut acc.keypair
                } else {
                    &mut acc.shared_obj
                };
                *field = curr.path();
                acc
            }),
    };

    writer.write_all(&toml::to_vec(&Config {
        program,
        ..Config::default()
    })?)?;

    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: String::from("https://localhost:8899"),
            program: Default::default(),
            options: Default::default(),
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            sleep: 100,
            timeout: 30,
            jobs: num_cpus::get(),
            max_retries: Some(9000),
        }
    }
}

impl Default for Program {
    fn default() -> Self {
        Self {
            authority: "~/.config/solana/id.json".parse().unwrap(),
            keypair: "./target/deploy/program-keypair.json".parse().unwrap(),
            shared_obj: "./target/deploy/program.so".parse().unwrap(),
        }
    }
}

// Config struct used by CLI.
pub struct AppConfig {
    pub url: Url,
    pub program_data: Vec<u8>,
    pub program_keypair: Keypair,
    pub authority: Keypair,
    pub send_config: RpcSendTransactionConfig,
    pub client: RpcClient,
    pub options: Options,
}

impl AppConfig {
    pub fn parse<P: AsRef<Path>>(p: P) -> Result<Self> {
        let config: Config = std::fs::read(p)
            .context("Failed to read config file.")
            .and_then(|c| {
                toml::from_slice(&c).context("Failed to parse config file.")
            })?;

        let expand_and_read_keypair = |p: &Path| -> Result<_> {
            read_keypair_file(shellexpand::full(&p.to_string_lossy())?.as_ref())
                .map_err(|e| {
                    anyhow!(
                        "Couldn't read keypair file ({}): {e}",
                        p.to_string_lossy()
                    )
                })
        };

        let client = RpcClient::new_with_timeouts_and_commitment(
            &config.url,
            Duration::from_secs(config.options.timeout),
            CommitmentConfig::confirmed(),
            Duration::from_secs(5),
        );

        let send_config = RpcSendTransactionConfig {
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            max_retries: config.options.max_retries,
            ..Default::default()
        };

        // TODO: setup multiple programs
        let program = &config.program;
        let authority = expand_and_read_keypair(&program.authority)
            .context("Couldn't read program authority keypair.")?;
        let program_keypair = expand_and_read_keypair(&program.keypair)
            .context("Couldn't read program keypair.")?;
        let program_data = read_and_verify_elf(&program.shared_obj)?;

        Ok(Self {
            options: config.options,
            url: Url::parse(&config.url)?,
            send_config,
            client,
            authority,
            program_data,
            program_keypair,
        })
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

    let chunk_sz = calculate_max_chunk_size(config, buffer_acc)?;
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
                let total_index = i * config.options.jobs + j;

                s.spawn(move |_| -> Result<()> {
                    let offset = (total_index * chunk_sz) as u32;
                    if offset >= program_data.len() as u32 {
                        return Ok(());
                    }

                    let mut stdout = std::io::stdout();

                    let bytes = chunks
                        .chunks(chunk_sz)
                        .nth(j)
                        .ok_or_else(|| anyhow!("Failed to read thread chunk"))?
                        .to_vec();
                    let msg = Message::new_with_blockhash(
                        &[write(&buffer_acc, &payer.pubkey(), offset, bytes)],
                        Some(&payer.pubkey()),
                        &blockhash,
                    );
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
