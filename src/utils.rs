use anyhow::{Context, Result};
use solana_bpf_loader_program::{
    syscalls::register_syscalls, BpfError, ThisInstructionMeter,
};
use solana_client::{
    client_error::ClientError, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig, rpc_response,
};
use solana_program_runtime::invoke_context::InvokeContext;
use solana_rbpf::{elf, verifier, vm};
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, message::Message,
    packet::PACKET_DATA_SIZE, signature::Signature, transaction::Transaction,
    transaction_context::TransactionContext,
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub fn calculate_max_chunk_size<F>(create_msg: &F) -> Result<usize>
where
    F: Fn(u32, Vec<u8>, Hash) -> Message,
{
    let baseline_msg = create_msg(0, Vec::new(), Hash::new_unique());
    let tx_size = bincode::serialized_size(&Transaction {
        signatures: vec![
            Signature::default();
            baseline_msg.header.num_required_signatures as usize
        ],
        message: baseline_msg,
    })? as usize;

    // add 1 byte buffer to account for shortvec encoding
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
    timeout: u64,
    sleep: u64,
) -> Result<Signature, ClientError> {
    loop {
        let hash = client.send_transaction_with_config(transaction, config)?;
        let start_time = Instant::now();

        loop {
            if let Ok(rpc_response::Response { value: true, .. }) =
                client.confirm_transaction_with_commitment(&hash, commitment)
            {
                return Ok(hash);
            }

            if Instant::now().duration_since(start_time).as_secs() > timeout {
                break;
            }

            std::thread::sleep(Duration::from_millis(sleep));
        }
    }
}
