use anyhow::{anyhow, ensure, Context, Result};
use crossbeam::thread;
use solana_sdk::{
    bpf_loader_upgradeable::{
        self, create_buffer, deploy_with_max_program_len, upgrade,
        UpgradeableLoaderState,
    },
    commitment_config::CommitmentConfig,
    message::Message,
    native_token::lamports_to_sol,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use std::time::Instant;

use crate::utils::*;
use crate::AppConfig;

pub fn create(
    config: &AppConfig,
    buffer_acc: &Keypair,
    buffer_len: usize,
) -> Result<()> {
    let min_balance = config
        .client
        .get_minimum_balance_for_rent_exemption(buffer_len)?;
    let payer_balance =
        config.client.get_balance(&config.authority.pubkey())?;

    println!(
        "Need {} SOL to create buffer account.\nCurrent balance is: {}",
        lamports_to_sol(min_balance),
        lamports_to_sol(payer_balance),
    );

    ensure!(payer_balance >= min_balance, "Insufficient funds.");

    let ix = create_buffer(
        &config.authority.pubkey(),
        &buffer_acc.pubkey(),
        &config.authority.pubkey(),
        min_balance,
        config.program_data.len(),
    )?;

    let blockhash = config.client.get_latest_blockhash()?;

    let tx = Transaction::new_signed_with_payer(
        &ix,
        Some(&config.authority.pubkey()),
        &[&config.authority, buffer_acc],
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

    Ok(())
}

pub fn write_data(
    config: &AppConfig,
    buffer_acc: Pubkey,
    buffer_len: usize,
) -> Result<()> {
    let payer = &config.authority;
    let client = &config.client;
    let program_data = &config.program_data;

    let chunk_sz = calculate_max_chunk_size(config, buffer_acc)?;
    let tx_count = buffer_len / chunk_sz + 2;

    let mut blockhash = client.get_latest_blockhash()?;
    let mut start_time = Instant::now();

    for (i, chunks) in program_data.chunks(chunk_sz * config.jobs).enumerate() {
        if start_time.elapsed().as_secs() > 30 {
            start_time = Instant::now();
            blockhash = client
                .get_latest_blockhash()
                .context("Couldn't get recent blockhash")?
        };

        let result = thread::scope(move |s| {
            for j in 0..config.jobs {
                let total_index = i * config.jobs + j;
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
                    let msg = Message::new_with_blockhash(
                        &[bpf_loader_upgradeable::write(
                            &buffer_acc,
                            &payer.pubkey(),
                            offset,
                            bytes,
                        )],
                        Some(&payer.pubkey()),
                        &blockhash,
                    );
                    let tx = Transaction::new(&[payer], msg, blockhash);
                    let tx_sig = send_and_confirm_transaction_with_config(
                        client,
                        &tx,
                        client.commitment(),
                        config.send_config,
                        config.timeout,
                        config.sleep,
                    )
                    .context("Write tx error.")?;

                    term_print(
                        format!(
                            "Confirmed ({}/{}): {}",
                            total_index + 2,
                            tx_count,
                            tx_sig
                        )
                        .as_ref(),
                    )?;

                    Ok(())
                });
            }
        });

        if result.is_err() {
            close(config, buffer_acc)?;
        }
    }

    Ok(())
}

pub fn deploy(config: &AppConfig, buffer_acc: Pubkey) -> Result<()> {
    let program = &config.program_keypair;
    let payer = &config.authority;

    let program_acc = config.client.get_account(&program.pubkey());
    let blockhash = config
        .client
        .get_latest_blockhash()
        .context("Couldn't get recent blockhash.")?;

    let tx = match program_acc {
        Err(_) => {
            println!("Deploying {}", program.pubkey());

            let program_lamports = config
                .client
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

    config
        .client
        .send_and_confirm_transaction_with_spinner_and_config(
            &tx,
            config.client.commitment(),
            config.send_config,
        )?;

    Ok(())
}

pub fn close(config: &AppConfig, buffer_acc: Pubkey) -> Result<()> {
    let blockhash = config
        .client
        .get_latest_blockhash()
        .context("Failed to fetch latest blockhash.")?;
    let close_ix = bpf_loader_upgradeable::close(
        &buffer_acc,
        &config.authority.pubkey(),
        &config.authority.pubkey(),
    );
    let close_tx = Transaction::new_signed_with_payer(
        &[close_ix],
        Some(&config.authority.pubkey()),
        &[&config.authority],
        blockhash,
    );

    config
        .client
        .send_and_confirm_transaction_with_spinner_and_config(
            &close_tx,
            config.client.commitment(),
            config.send_config,
        )
        .context("Unable to close the buffer")?;

    Ok(())
}
