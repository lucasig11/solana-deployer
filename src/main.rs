use anyhow::{bail, Context, Result};
use clap::Parser;
use solana_sdk::signer::Signer;
use std::{fs::File, path::PathBuf, time::Instant};

use solana_deployer::*;

#[derive(Parser)]
pub struct Args {
    #[clap(short, long = "config", default_value = "deploy.toml")]
    /// Path to the deploy configuration file.
    config_path: PathBuf,
    #[clap(subcommand)]
    subcommands: Option<SubCommands>,
}

#[derive(clap::Subcommand)]
enum SubCommands {
    /// Generates a deploy.toml file in your CWD with the default values.
    GenConfig {
        #[clap(short, long)]
        /// Output filename.
        output: Option<String>,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(SubCommands::GenConfig { output }) = args.subcommands {
        let cwd = std::env::current_dir()?;
        if let Some(filename) = output {
            let mut fd = File::options()
                .write(true)
                .create_new(true)
                .open(&filename)
                .context("Failed to create config file.")?;
            println!("Writing contents to {}.", filename);
            return generate_config(&mut fd, &cwd);
        }
        return generate_config(&mut std::io::stdout(), &cwd);
    }

    let config = AppConfig::parse(args.config_path)?;
    let start_ts = Instant::now();

    // Create new buffer account.
    let (buffer_acc, buffer_sz) = create_buffer_account(&config)?;

    // Write to buffer account.
    write_to_buffer_account(&config, buffer_acc.pubkey(), buffer_sz)?;

    // Deploy/upgrade program.
    if let Err(e) = deploy_or_upgrade_program(&config, buffer_acc.pubkey()) {
        close_buffer_account(&config, buffer_acc.pubkey())?;
        bail!(e);
    }

    println!("✅ Success! Completed in {}s", start_ts.elapsed().as_secs());

    Ok(())
}
