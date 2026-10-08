//! siphon: stream an S3 bucket into a Proxmox Backup Server snapshot, and back.
//!
//! See docs/DESIGN.md for the data flow.

mod backup;
mod cli;
mod restore;
mod s3;
mod tree;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match Cli::parse().command {
        Command::Backup(args) => backup::run(args).await,
        Command::Restore(args) => restore::run(args).await,
    }
}
