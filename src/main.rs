//! siphon: stream an S3 bucket into a Proxmox Backup Server snapshot, and back.
//!
//! See docs/DESIGN.md for the data flow.

mod backup;
mod cli;
mod healthchecks;
mod restore;
mod s3;
mod tree;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use healthchecks::Healthchecks;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let hc = cli.hc_ping_url.as_deref().map(Healthchecks::new);
    if let Some(hc) = &hc {
        hc.start().await;
    }

    let result = match cli.command {
        Command::Backup(args) => backup::run(args).await,
        Command::Restore(args) => restore::run(args).await,
    };

    if let Some(hc) = &hc {
        match &result {
            Ok(summary) => hc.success(summary).await,
            Err(err) => hc.fail(&format!("{err:?}")).await,
        }
    }
    result.map(drop)
}
