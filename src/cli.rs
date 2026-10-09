//! Command line. Every option can also be set through the environment
//! variable shown in `--help`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use pbs_api_types::{BackupNamespace, BackupType};
use pbs_client::{BackupRepository, HttpClient, HttpClientOptions};

#[derive(Parser)]
#[command(version, about = "Back up an S3 bucket into Proxmox Backup Server")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Healthchecks ping URL (`https://hc-ping.com/<uuid>`): pinged with
    /// `/start` before the run, then with the summary or `/fail` and the error.
    #[arg(long, env = "HC_PING_URL", hide_env_values = true, global = true)]
    pub hc_ping_url: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Stream all objects of a bucket into a new PBS snapshot.
    Backup(BackupArgs),
    /// Upload the objects of a PBS snapshot back into a bucket.
    Restore(RestoreArgs),
}

#[derive(Args)]
pub struct PbsArgs {
    /// PBS repository, `[[auth-id@]server[:port]:]datastore`.
    #[arg(long, env = "PBS_REPOSITORY", value_parser = parse_repository)]
    pub repository: String,

    /// Password or API token secret.
    #[arg(long, env = "PBS_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// Read the password or API token secret from this file (first line).
    #[arg(long, env = "PBS_PASSWORD_FILE", conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    /// Expected TLS certificate fingerprint of the PBS server.
    #[arg(long, env = "PBS_FINGERPRINT")]
    pub fingerprint: Option<String>,

    /// Backup namespace (default: root namespace).
    #[arg(long, env = "PBS_NAMESPACE", default_value_t)]
    pub ns: BackupNamespace,
}

fn parse_repository(s: &str) -> Result<String> {
    s.parse::<BackupRepository>()?;
    Ok(s.to_string())
}

impl PbsArgs {
    pub fn repository(&self) -> BackupRepository {
        self.repository.parse().expect("validated by clap")
    }

    fn password(&self) -> Result<String> {
        if let Some(password) = &self.password {
            return Ok(password.clone());
        }
        if let Some(path) = &self.password_file {
            let data = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            return Ok(data.lines().next().unwrap_or_default().to_string());
        }
        bail!("no PBS password given (--password, --password-file)");
    }

    pub fn connect(&self) -> Result<HttpClient> {
        let repo = self.repository();
        let options =
            HttpClientOptions::new_non_interactive(self.password()?, self.fingerprint.clone());
        HttpClient::new(repo.host(), repo.port(), repo.auth_id(), options)
            .with_context(|| format!("connecting to {repo}"))
    }
}

#[derive(Args)]
pub struct S3Args {
    /// S3 endpoint URL, e.g. `https://s3.example.com` (default: AWS).
    #[arg(long = "s3-endpoint", env = "S3_ENDPOINT")]
    pub endpoint: Option<String>,

    /// S3 region.
    #[arg(long = "s3-region", env = "S3_REGION", default_value = "us-east-1")]
    pub region: String,

    /// Use path-style bucket addressing (`endpoint/bucket/key`) instead of
    /// virtual-hosted style (`bucket.endpoint/key`).
    #[arg(long = "s3-path-style", env = "S3_PATH_STYLE")]
    pub path_style: bool,

    /// Bucket to back up from, or to restore into.
    #[arg(long = "s3-bucket", env = "S3_BUCKET")]
    pub bucket: String,

    /// Only handle keys starting with this prefix.
    #[arg(long = "s3-prefix", env = "S3_PREFIX")]
    pub prefix: Option<String>,

    #[arg(long = "s3-access-key-id", env = "AWS_ACCESS_KEY_ID")]
    pub access_key_id: String,

    #[arg(
        long = "s3-secret-access-key",
        env = "AWS_SECRET_ACCESS_KEY",
        hide_env_values = true
    )]
    pub secret_access_key: String,
}

#[derive(Args)]
pub struct BackupArgs {
    #[command(flatten)]
    pub pbs: PbsArgs,

    #[command(flatten)]
    pub s3: S3Args,

    /// Backup type of the snapshot group.
    #[arg(long, env = "PBS_BACKUP_TYPE", default_value_t = BackupType::Host)]
    pub backup_type: BackupType,

    /// Backup ID of the snapshot group (default: bucket name).
    #[arg(long, env = "PBS_BACKUP_ID")]
    pub backup_id: Option<String>,

    /// Archive name without `.pxar` (default: bucket name).
    #[arg(long, env = "PBS_ARCHIVE")]
    pub archive: Option<String>,

    /// Don't fail when a body doesn't match its MD5 ETag.
    #[arg(long, env = "SIPHON_SKIP_ETAG_CHECK")]
    pub skip_etag_check: bool,
}

#[derive(Args)]
pub struct RestoreArgs {
    #[command(flatten)]
    pub pbs: PbsArgs,

    #[command(flatten)]
    pub s3: S3Args,

    /// Snapshot (`host/<id>/<time>`) or group (`host/<id>`, latest snapshot).
    #[arg(long, env = "PBS_SNAPSHOT")]
    pub snapshot: String,

    /// Archive name without `.pxar` (default: the backup ID of the snapshot).
    #[arg(long, env = "PBS_ARCHIVE")]
    pub archive: Option<String>,

    /// Write into a bucket (or prefix) that already contains objects.
    #[arg(long)]
    pub overwrite: bool,

    /// Only list what would be uploaded.
    #[arg(long)]
    pub dry_run: bool,

    /// Size of multipart upload parts; larger objects use multipart upload.
    #[arg(long, env = "SIPHON_PART_SIZE", default_value_t = 64 << 20)]
    pub part_size: usize,
}

/// `<name>.pxar.didx`, as the archive is called on the server.
pub fn archive_name(name: &str) -> Result<pbs_api_types::BackupArchiveName> {
    format!("{name}.pxar.didx")
        .as_str()
        .try_into()
        .with_context(|| format!("invalid archive name {name:?}"))
}

/// `[ns]:` for log lines, empty for the root namespace.
pub fn ns_prefix(ns: &BackupNamespace) -> String {
    if ns.is_root() {
        String::new()
    } else {
        format!("[{ns}]:")
    }
}
