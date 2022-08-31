use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use solana_client::{
    client_error::reqwest::Url, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    bpf_loader_upgradeable::UpgradeableLoaderState,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

mod buffer_account;
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
    pub shared_obj: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Options {
    pub jobs: usize,
    pub max_retries: Option<usize>,
    pub sleep: u64,
    pub timeout: u64,
}

pub fn run(config_path: &Path) -> Result<()> {
    let config = AppConfig::parse(config_path)?;
    let buffer_acc = Keypair::new();
    let buffer_len =
        UpgradeableLoaderState::buffer_len(config.program_data.len())?;

    // Create new buffer account.
    buffer_account::create(&config, &buffer_acc, buffer_len)?;

    // Write program data to buffer account.
    buffer_account::write_data(&config, buffer_acc.pubkey(), buffer_len)?;

    // Deploy/upgrade program.
    if let Err(e) = buffer_account::deploy(&config, buffer_acc.pubkey()) {
        buffer_account::close(&config, buffer_acc.pubkey())?;
        bail!(e);
    }

    Ok(())
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
