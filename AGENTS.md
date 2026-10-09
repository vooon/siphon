# AGENTS.md

Engineering notes for siphon: a Rust tool that streams an S3 bucket into a
Proxmox Backup Server snapshot via Proxmox's own client crates.

## Source of truth
- `docs/DESIGN.md` is the authoritative design. `README.md` is the public summary.
- User instructions in chat override both unless they conflict with safety constraints.

## CRITICAL: sanitization mandate
This is a **PUBLIC** repository; it is deployed into a **private** homelab. Never expose, in any file, commit, comment, doc, CI, test, or example:
- Real IPs, hostnames, domains, cluster/node names, bucket names of real deployments, PBS datastore/namespace names, or credentials.
- Use only placeholders: domains under `example.com`/`example.internal`,
  RFC documentation ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`),
  generic names (`pbs.example.com`, `s3.example.com`, `my-bucket`, `store`).
- Before committing, grep for literal `[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+` and real identifiers, and replace with placeholders.
- Never commit any key, token, fingerprint of a real server, or certificate material.

## Architecture
- One binary, `siphon`, subcommands `backup` (one run = one PBS snapshot) and
  `restore` (snapshot -> bucket). Every option is a clap flag with an `env`
  mapping (see docs/DESIGN.md → Configuration); meant to run as a CronJob.
- Modules: `cli.rs` (clap), `s3.rs` (client, listing, metadata <-> xattrs),
  `tree.rs` (key <-> archive path, escaping), `backup.rs`, `restore.rs`,
  `healthchecks.rs` (pings around every run; ping failures never fail a run).
- Data flow: S3 `ListObjectsV2` → per object `GetObject` body stream →
  `pxar` encoder → `pbs_client` chunker/`BackupWriter` → manifest → finish.
  Constant memory: one object body in flight; no local staging.
- S3 access: `aws-sdk-s3` (path-style or virtual-hosted). Object metadata is
  kept as `user.s3.*` xattrs; escaped keys carry `user.s3.key`.
- PBS access: `pbs-client` / `pbs-datastore` from the `proxmox-backup` git repo,
  pinned to a revision matching the target server release.
- `proxmox-backup-client`'s `backup` command is the reference for the call
  sequence; copy its order of operations rather than inventing one.

## Stack & conventions
- Rust, edition 2024, tokio. Release builds are glibc (Debian trixie), not musl:
  `pbs-client` links libacl, libsystemd, libuuid, libcrypt, OpenSSL, zstd.
- Proxmox crates are **not** on crates.io. They come from `git.proxmox.com`
  via git dependencies plus a `[patch.crates-io]` block in `Cargo.toml`
  (patches only take effect in the root workspace). When bumping the
  `proxmox-backup` rev, bump the `proxmox.git`/`pxar`/`pathpatterns` revs to
  what that release builds against, and keep `Cargo.lock` committed.
- Errors: `anyhow` at the binary edge. Logging: `log` + `env_logger`.
- Container image: `Dockerfile` (builder `rust:1-trixie`, runtime
  `debian:trixie-slim`, non-root). Published to `ghcr.io/vooon/siphon` by
  GitHub Actions (`.github/workflows/ci.yml`).
- Versioning: `bump2version {patch|minor|major}` updates `Cargo.toml` and
  `Cargo.lock`, commits and tags `v<version>`; pushing the tag publishes the
  semver image tags.

## Commands
- Build: `cargo build --release` (needs `libacl1-dev libcrypt-dev libssl-dev
  libsystemd-dev libzstd-dev uuid-dev libclang-dev pkg-config`)
- Verify: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`
- Image: `docker build -t siphon:e2e .` (or `podman build`)
- Host builds need the `-dev` libs above; without them, run tests in the
  builder stage: `docker build --target build -t siphon-build .` and
  `docker run --rm -v "$PWD":/w -w /w siphon-build cargo test`
- E2E: `SIPHON_IMAGE=siphon:e2e e2e/run.sh` (needs docker/podman compose,
  aws CLI v2, jq, python3; `KEEP=1` keeps the containers)
- Release: `bump2version patch && git push --follow-tags` (CI pushes the image,
  `release.yml` creates the GitHub release with git-cliff notes)

## Critical rules
- User directives are absolute: if the user says `DO NOT <action>`, do not perform that action without explicit permission.
- Never edit credential/config files or fabricate/overwrite credentials unless the user explicitly asks.
- Preserve user data and configuration; ask before changing if in doubt. Do not undo user choices in favor of your own approach without discussing first.
- `backup` must only **read** from S3. Only `restore` writes, only to its target bucket, and never deletes objects.
- Keep links absolute unless requested otherwise; prefer minimal, targeted patches.
- Every text file must end with a final newline (`\n`) — no trailing-newline-less files.

## Commit messages
- Conventional Commits: `<type>(<scope>): <description>` (see https://www.conventionalcommits.org/en/v1.0.0/#summary).
