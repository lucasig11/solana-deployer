use anyhow::{Context, Result};
use crossterm::{cursor, queue, terminal};
use solana_bpf_loader_program::{
    syscalls::register_syscalls, BpfError, ThisInstructionMeter,
};
use solana_client::{
    client_error::ClientError, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig, rpc_response::Response,
};
use solana_program_runtime::invoke_context::InvokeContext;
use solana_rbpf::{elf, verifier, vm};
use solana_sdk::{
    bpf_loader_upgradeable, commitment_config::CommitmentConfig, hash::Hash,
    message::Message, packet::PACKET_DATA_SIZE, pubkey::Pubkey,
    signature::Signature, signer::Signer, transaction::Transaction,
    transaction_context::TransactionContext,
};
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

use crate::AppConfig;

pub fn calculate_max_chunk_size(
    config: &AppConfig,
    buffer_acc: Pubkey,
) -> Result<usize> {
    let baseline_msg = Message::new_with_blockhash(
        &[bpf_loader_upgradeable::write(
            &buffer_acc,
            &config.authority.pubkey(),
            0,
            vec![],
        )],
        Some(&config.authority.pubkey()),
        &Hash::new_unique(),
    );

    let tx_size = bincode::serialized_size(&Transaction {
        signatures: vec![
            Signature::default();
            baseline_msg.header.num_required_signatures as usize
        ],
        message: baseline_msg,
    })? as usize;

    // Add a 1-byte-buffer to account for shortvec encoding.
    Ok(PACKET_DATA_SIZE.saturating_sub(tx_size).saturating_sub(1))
}

pub fn read_and_verify_elf<P: AsRef<Path>>(path: &P) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let mut transaction_context = TransactionContext::new(vec![], 1, 1);
    let mut invoke_context =
        InvokeContext::new_mock(&mut transaction_context, &[]);

    // Verify the program
    elf::Executable::<BpfError, ThisInstructionMeter>::from_elf(
        &bytes,
        Some(verifier::check),
        vm::Config {
            reject_broken_elfs: true,
            ..vm::Config::default()
        },
        register_syscalls(&mut invoke_context)?,
    )
    .context("ELF error: {}")?;

    Ok(bytes)
}

pub fn send_and_confirm_transaction_with_config(
    client: &RpcClient,
    transaction: &Transaction,
    commitment: CommitmentConfig,
    config: RpcSendTransactionConfig,
    timeout: Duration,
    sleep: Duration,
) -> Result<Signature, ClientError> {
    loop {
        let hash = client.send_transaction_with_config(transaction, config)?;
        let start_time = Instant::now();

        loop {
            if let Ok(Response { value: true, .. }) =
                client.confirm_transaction_with_commitment(&hash, commitment)
            {
                return Ok(hash);
            }
            if start_time.elapsed() > timeout {
                break;
            }

            std::thread::sleep(sleep);
        }
    }
}

pub fn term_print(s: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    queue!(stdout, cursor::SavePosition)?;
    stdout.write_all(s.as_ref())?;
    queue!(stdout, cursor::RestorePosition)?;
    stdout.flush()?;
    queue!(
        stdout,
        cursor::RestorePosition,
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;
    Ok(())
}
