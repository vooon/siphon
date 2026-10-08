# siphon — design

Back up an S3 bucket (e.g. a Ceph RGW bucket holding CloudNativePG/Barman base
backups + WAL) straight into Proxmox Backup Server, so it reaches PBS
retention and tape — without a local copy of the bucket, without patching
`proxmox-backup-client`. Status: skeleton builds against `pbs-client` 4.2.3;
data path not implemented yet (2026-10-08).

## Why a dedicated tool

| Option | Problem |
|---|---|
| `rclone sync` to CephFS, then `proxmox-backup-client` | the bucket exists twice on Ceph |
| `rclone mount` + `proxmox-backup-client` | FUSE privileges in the pod, fragile on large reads |
| Go client (`tizbac/proxmoxbackupclient_go`) | third-party protocol reimplementation; maturity unknown |
| **Rust tool on Proxmox's own crates** | same chunker/index/protocol code as the official client |

## Building against Proxmox crates

Proxmox's crates aren't usable from crates.io, so they all come from
`git.proxmox.com` (verified: `cargo build` of the skeleton works, the
container image builds):

- `pbs-client` and `pbs-datastore` are members of the `proxmox-backup`
  workspace and are taken as git dependencies pinned to a commit. Proxmox
  doesn't tag every release (no `v4.2.3`…`v4.2.8` tags; only
  `bump version to 4.2.x-1` commits), so pin by `rev`:

  ```toml
  pbs-client    = { git = "https://git.proxmox.com/git/proxmox-backup.git", rev = "bc863d45e" } # 4.2.3-1
  pbs-datastore = { git = "https://git.proxmox.com/git/proxmox-backup.git", rev = "bc863d45e" }
  ```

  Pick the commit matching the PBS server (or older), `git log --grep 'bump version'`.
- The `proxmox-*`, `pbs-api-types`, `pxar` and `pathpatterns` crates are
  pinned through `[patch.crates-io]` to one revision of each repo
  (`proxmox.git`, `pxar.git`, `pathpatterns.git`), so our direct deps and
  `pbs-client`'s own deps resolve to the same code. `[patch]` only applies
  in the root workspace, so it lives in siphon's `Cargo.toml`, not in the
  dependency. Cargo warns about unused patches; drop those.
- Considered and dropped: the Debian `devel` repository
  (`librust-*-dev` into `/usr/share/cargo/registry` + source replacement),
  which ties the build to a Debian host image and Debian's rustc; and vendoring
  `proxmox-backup` as a submodule, which adds checkout weight for no gain over
  a pinned git dependency.
- Native libraries linked by `pbs-client`: libacl, libcrypt, libssl,
  libsystemd, libuuid, libzstd (bindgen → libclang at build time).
  Hence a glibc build on Debian trixie, no musl/static binary.
- Licence: `proxmox-backup` is AGPL-3.0, so siphon is too.

Sources: https://git.proxmox.com/?p=proxmox-backup.git ,
https://git.proxmox.com/?p=pxar.git , https://git.proxmox.com/?p=proxmox.git ,
protocol: https://pbs.proxmox.com/docs/backup-protocol.html

## Data flow

```
S3 ListObjectsV2 (sorted keys)
  └─ per object: GetObject body (async stream)
       └─ pxar::encoder (aio): directory entries for key prefixes,
          create_file(metadata, name, size) <- copy the body in
             └─ byte stream ─▶ pbs_client::ChunkStream (dynamic chunking)
                  └─ BackupWriter::upload_stream("<archive>.pxar.didx")
  └─ manifest (index.json.blob) ─▶ BackupWriter::finish()
```

- Memory stays constant: one object body in flight at a time.
- Archive tree: `<bucket>/<object key>` as directories and files.
  Metadata: `mtime` = S3 `LastModified`, size from the listing, mode `0644`
  (`0755` for directories), owner 0:0. Optionally the ETag as an xattr
  (`user.s3.etag`) for verification.
- PBS layout, like other file backups: namespace from `PBS_NAMESPACE`,
  group `host/<PBS_BACKUP_ID>`, archive `<PBS_ARCHIVE>.pxar`.
- S3 client: `proxmox-s3-client` (already in the dependency graph; PBS uses it
  for S3 datastores). It has `list_objects_v2` (with continuation token),
  `get_object` (streamed body), path-style addressing and a fingerprint
  option for self-signed endpoints. To verify: its
  `S3_HTTP_REQUEST_TIMEOUT` (30 min) must not cut off a long body read of a
  large base backup.

API names to verify against the pinned revision (they move between releases):
`pbs_client::{HttpClient, HttpClientOptions, BackupRepository,
BackupWriter, ChunkStream, UploadOptions}`, `BackupWriter::start(...)`,
`upload_stream(...)`, `upload_blob_from_data(...)` for the manifest,
`pxar::encoder::aio::Encoder::{create_file, create_directory, finish}`.
`proxmox-backup-client`'s own `backup` command (`proxmox-backup-client/src/main.rs`)
is the reference for the call sequence.

## Phases

1. **Full read each run.** Stream every object; PBS deduplication keeps
   storage flat (Barman objects never change), but every run re-downloads
   the whole bucket. Acceptable while the bucket is small.
2. **Reuse unchanged objects** (like the client's `--change-detection-mode=metadata`):
   split archive (`.mpxar` metadata + `.ppxar` payload). For each object whose
   key, size and ETag/LastModified match the previous snapshot's metadata
   archive, reference the previous payload chunks instead of downloading the
   object (the client's "payload reuse / injection" path). Only new WAL and
   new base backups get read. This is the main reason to build on
   `pbs-client` rather than reimplementing the protocol.

## Configuration (environment)

`PBS_REPOSITORY`, `PBS_PASSWORD` (API token secret), `PBS_FINGERPRINT`,
`PBS_NAMESPACE`, `PBS_BACKUP_ID`, `PBS_ARCHIVE` (default: bucket name),
`S3_ENDPOINT` (e.g. `https://s3.example.com`), `S3_BUCKET`, `S3_PREFIX` (optional),
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (a **read-only** user),
`HC_PING_URL` (Healthchecks `/start`, `/<exit code>`), `GOTIFY_*` (optional).

## Consistency and restore

- Barman writes objects once and never modifies them; a run is a consistent
  set of "base backups + WAL up to the listing time". An object deleted
  between listing and reading (retention cleanup) is skipped with a warning.
- Restore: `proxmox-backup-client restore <snapshot> <archive>.pxar <dir>`
  (or single files from the PBS UI), then upload the directory to a bucket
  (`rclone copy`) and point a CNPG `Cluster` recovery at it.

## Build, release, deployment

- Source and CI on GitHub (`vooon/siphon`). GitHub Actions
  (`.github/workflows/ci.yml`): fmt, clippy, tests; then the image is built
  with Buildx and pushed to `ghcr.io/vooon/siphon` on `main` and on `v*` tags
  (PRs build only). Tags: branch, `sha-…`, and `X.Y.Z` / `X.Y` from the git tag.
- `Dockerfile`: builder `rust:1-trixie` (+ the `-dev` libs above), runtime
  `debian:trixie-slim` with the matching shared libraries, runs as nobody.
- Versioning: `bump2version` (`.bumpversion.cfg`) bumps `Cargo.toml` and
  `Cargo.lock`, commits and tags `vX.Y.Z`.
- Deployment (private homelab GitOps repo, not here): nightly CronJob after
  the CNPG base backups, image tracked by Renovate; secrets in SOPS;
  a Healthchecks check. Source bucket access through a read-only user with
  `s3:GetObject`/`s3:ListBucket` only.

## Testing

- Unit: pxar encoding of a fake object list; restore and compare with
  `proxmox-backup-client restore`.
- Integration: a test bucket and a test namespace on PBS; restore, then a
  CNPG recovery from the restored copy.
