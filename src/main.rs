//! siphon: stream an S3 bucket into a Proxmox Backup Server snapshot.
//!
//! Stub: wiring only. See docs/DESIGN.md for the data flow.

use anyhow::{Context, Result};
use pbs_client::BackupRepository;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let repo: BackupRepository = std::env::var("PBS_REPOSITORY")
        .context("PBS_REPOSITORY is not set")?
        .parse()
        .context("invalid PBS_REPOSITORY")?;
    log::info!("target repository: {repo}");

    anyhow::bail!("not implemented yet");
}
