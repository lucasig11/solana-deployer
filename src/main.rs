use anyhow::{bail, Result};
use clap::Parser;
use solana_sdk::signer::Signer;
use std::path::PathBuf;
use std::time::Instant;

use solana_deployer::*;

#[derive(Parser)]
pub struct Args {
    #[clap(short, long = "config", default_value = "deploy.toml")]
    /// Path to the deploy configuration file.
    config_path: PathBuf,
}

fn main() -> Result<()> {
    let cli_args = Args::parse();
    let config = AppConfig::parse(cli_args.config_path)?;
    let start_ts = Instant::now();

    // Create new buffer account.
    let (buffer_kp, buffer_len) = create_buffer_account(&config)?;

    // Write to buffer account.
    write_to_buffer_account(&config, buffer_kp.pubkey(), buffer_len)?;

    // Deploy/upgrade program.
    if let Err(e) = deploy_or_upgrade_program(&config, buffer_kp.pubkey()) {
        close_buffer_account(&config, buffer_kp.pubkey())?;
        bail!(e);
    }

    Ok(println!(
        "✅ Success! Completed in {}s",
        start_ts.elapsed().as_secs()
    ))
}
