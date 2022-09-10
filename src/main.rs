use anyhow::{Context, Result};
use clap::Parser;
use std::{fs::File, path::PathBuf, time::Instant};

use solana_deployer::*;

#[derive(Parser)]
/// Deploy Solana programs during high load.
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
    let start_ts = Instant::now();

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

    match run(&args.config_path) {
        Ok(_) => println!(
            "✅ Success! Completed in {}s",
            start_ts.elapsed().as_secs()
        ),
        Err(e) => eprintln!("{e}"),
    };

    Ok(())
}
