# siphon — design

Back up an S3 bucket (e.g. a Ceph RGW bucket holding CloudNativePG/Barman base
backups + WAL) straight into Proxmox Backup Server, so it reaches PBS
retention and tape — without a local copy of the bucket, without patching
`proxmox-backup-client`. And restore it back into a bucket, metadata included.

Status (2026-10-08): phase 1 (full read each run) works end to end against
rustfs and a containerised PBS 4.2 (`e2e/run.sh`). Not deployed yet.

## Why a dedicated tool

| Option | Problem |
|---|---|
| `rclone sync` to CephFS, then `proxmox-backup-client` | the bucket exists twice on Ceph |
| `rclone mount` + `proxmox-backup-client` | FUSE privileges in the pod, fragile on large reads |
| Go client (`tizbac/proxmoxbackupclient_go`) | third-party protocol reimplementation; maturity unknown |
| **Rust tool on Proxmox's own crates** | same chunker/index/protocol code as the official client |

## Building against Proxmox crates

Proxmox's crates aren't usable from crates.io, so they all come from
`git.proxmox.com`:

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

## S3 client

`aws-sdk-s3` (rustls). `proxmox-s3-client` was the first choice since it is
already in the dependency graph, but it drops every response header except
size, ETag, Content-Type and Last-Modified (so no `x-amz-meta-*`), can't set
metadata on upload and has no multipart upload — restore needs all three.

- Path-style (`endpoint/bucket/key`) or virtual-hosted style
  (`bucket.endpoint/key`, the default): `--s3-path-style`.
- Checksum trailers are only sent/validated when an operation requires them
  (`WhenRequired`), since not every S3-compatible store supports the SDK's
  newer defaults.
- TLS roots come from the system store; a private CA can be added with
  `SSL_CERT_FILE`.

## Backup data flow

```
S3 ListObjectsV2 (all pages) ─▶ key tree (tree.rs)
  └─ per file, depth first: GetObject (body stream + headers)
       └─ pxar::encoder::aio: create_directory / create_file(metadata+xattrs, size)
          <- body copied in, MD5 checked against a plain ETag
             └─ pipe ─▶ pbs_client::ChunkStream (dynamic chunking)
                  └─ BackupWriter::upload_stream("<archive>.pxar.didx")
  └─ catalog (in memory) ─▶ upload_stream("catalog.pcat1.didx")
  └─ manifest (index.json.blob) ─▶ BackupWriter::finish()
```

- One object body in flight; no local staging. The key list is held in
  memory (fine for tens of thousands of keys).
- Any error drops the upload before `finish`, so PBS discards the snapshot.
- Catalog: the PBS web UI browses a unified `.pxar` archive through
  `catalog.pcat1.didx` (one directory named `<archive>.pxar.didx`, then the
  tree with sizes and mtimes), as `proxmox-backup-client` writes it in the
  default change-detection mode. Split archives (`.mpxar`/`.ppxar`, phase 2)
  are browsed from the metadata archive and need no catalog. Built in memory
  (tens of bytes per entry) and uploaded after the archive.
- An object deleted between listing and reading (retention cleanup) is
  skipped with a warning. Barman never modifies objects, so a run is a
  consistent set of "base backups + WAL up to the listing time".
- PBS layout: namespace `--ns`, group `<--backup-type>/<--backup-id>`
  (default `host/<bucket>`), archive `<--archive>.pxar` (default bucket name).
  Unencrypted for now.

### Archive tree

The archive root is the bucket root: key `a/b/c` is file `c` in directory
`a/b`. Files are mode `0644`, directories `0755`, owner 0:0; file mtime is
S3 `LastModified`, directory mtime the newest file below it.

Keys that can't be a path are stored flat as
`.siphon/keys/<sha256(key)>` with the real key in `user.s3.key`:
empty components (`a//b`, `dir/` folder markers, leading `/`), `.`/`..`
components, components over 255 bytes, keys under `.siphon/`, and a key that
is also a prefix of other keys (`a` next to `a/b`: the file `a` is escaped).

### Metadata as xattrs

Taken from the `GetObject` response headers, so no extra request:

| xattr | from |
|---|---|
| `user.s3.etag` | `ETag` |
| `user.s3.version-id` | `x-amz-version-id` (absent / `null` on unversioned buckets) |
| `user.s3.content-type`, `…content-encoding`, `…content-disposition`, `…content-language`, `…cache-control` | same-named headers |
| `user.s3.meta.<name>` | `x-amz-meta-<name>` |
| `user.s3.key` | original key, escaped entries only |

All under `user.` so `proxmox-backup-client restore` to a directory keeps
them. Not kept: tags (extra request per object), storage class, SSE settings.

## Restore

`siphon restore --snapshot host/<id>[/<time>]` (a group means its latest
finished snapshot) reads the archive through `BackupReader` +
`pxar::accessor::aio`, and uploads every file to `--s3-bucket` under its key
(`user.s3.key` if set, else its path), with Content-* headers and user
metadata from the xattrs.

- Objects up to `--part-size` (64 MiB) are buffered and sent with PutObject;
  larger ones use multipart upload with buffered parts (aborted on error).
- Refuses a bucket (or `--s3-prefix`) that already has objects unless
  `--overwrite`; `--dry-run` only lists keys and sizes.
- Not restorable by any S3 client: `LastModified`, version IDs, and the ETag
  of multipart objects. For single-part objects the new ETag is compared with
  the old one (warning on mismatch).
- Also possible without siphon: `proxmox-backup-client restore <snapshot>
  <archive>.pxar <dir>` and `rclone copy` — loses the metadata.

## Phases

1. **Full read each run** (done). Stream every object; PBS deduplication keeps
   storage flat, but every run re-downloads the whole bucket.
2. **Reuse unchanged objects** (later; like the client's
   `--change-detection-mode=metadata`): split archive (`.mpxar` metadata +
   `.ppxar` payload). An object whose key, size, ETag and LastModified (all in
   the listing) match the previous snapshot's metadata archive gets no S3
   request: its entry and xattrs are copied from the previous `.mpxar` and its
   payload references the previous chunks. Only new WAL and base backups are
   read. The work: `pbs_client::pxar::create_archive`'s reuse/injection logic
   walks a real directory, so it has to be rebuilt for our key tree.
3. **All versions** (maybe): `ListObjectVersions`, older versions in a side
   tree (`<key>/.versions/<mtime>_<version-id>`), restored oldest first.

## Configuration

Every option is a flag and an environment variable (`siphon <cmd> --help`).

| Variable | Flag | Notes |
|---|---|---|
| `PBS_REPOSITORY` | `--repository` | `user@realm!token@host[:port]:datastore` |
| `PBS_PASSWORD` / `PBS_PASSWORD_FILE` | `--password` / `--password-file` | API token secret |
| `PBS_FINGERPRINT` | `--fingerprint` | for a self-signed PBS certificate |
| `PBS_NAMESPACE` | `--ns` | default: root |
| `PBS_BACKUP_TYPE`, `PBS_BACKUP_ID` | `--backup-type`, `--backup-id` | backup; default `host`, bucket name |
| `PBS_ARCHIVE` | `--archive` | default: bucket name (backup), backup ID (restore) |
| `PBS_SNAPSHOT` | `--snapshot` | restore |
| `S3_ENDPOINT`, `S3_REGION`, `S3_PATH_STYLE` | `--s3-endpoint`, `--s3-region`, `--s3-path-style` | region default `us-east-1` |
| `S3_BUCKET`, `S3_PREFIX` | `--s3-bucket`, `--s3-prefix` | |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | `--s3-access-key-id`, `--s3-secret-access-key` | a read-only user for backup |
| `SIPHON_PART_SIZE` | `--part-size` | restore |
| `SIPHON_SKIP_ETAG_CHECK` | `--skip-etag-check` | backup |
| `HC_PING_URL` | `--hc-ping-url` | Healthchecks pings, see below |
| `RUST_LOG` | | default `info` |

### Healthchecks

With `HC_PING_URL` set (any command), siphon pings `<url>/start` before the
run, then `<url>` with the summary line as body on success, or `<url>/fail`
with the error chain on failure; all with the same `?rid=<uuid>` so
Healthchecks pairs them and measures the run time. Bodies are cut at 100 kB
(Healthchecks' limit). A ping that fails is retried twice (10 s timeout each)
and then only logged — it never fails the run. HTTP via `proxmox-http`
(OpenSSL, system CAs). Gotify and other notifications: use Healthchecks'
integrations.

## Build, test, release

- GitHub Actions `ci.yml`: fmt, clippy, unit tests → e2e → image pushed to
  `ghcr.io/vooon/siphon` (on `main` and `v*` tags; PRs build only).
- e2e (`e2e/run.sh`, compose): rustfs (pinned) and
  `ayufan/proxmox-backup-server` (unofficial PBS image, pinned) on
  localhost; bootstraps a datastore and API token with
  `proxmox-backup-manager`, seeds a bucket (metadata, a multipart-uploaded
  object, escaped keys, Unicode), runs backup twice, PBS verify, restore
  (dry run, real, refusal without `--overwrite`) and compares every body and
  header. The web UI's catalog listing and single-file download are checked
  through the PBS API. A Python stand-in (`e2e/hc-mock.py`) checks the Healthchecks pings:
  start/success/fail pairing by run ID and the message bodies. rustfs rejects `a//b`, `./x`, `a/../b`; those are covered by unit
  tests only. The PBS image has no `proxmox-backup-client`, so restore with
  the official client isn't tested.
- Release: `bump2version` → tag `vX.Y.Z` → `release.yml` checks the tag
  against `Cargo.toml`, builds the binary (Dockerfile `artifact` stage,
  linux-amd64, glibc/trixie), writes notes with git-cliff from Conventional
  Commits, and creates the GitHub release. The image comes from `ci.yml`.
- Dependabot: cargo, actions, Dockerfile, e2e compose; weekly, 7-day cooldown.

## Deployment (private GitOps repo, not here)

Nightly CronJob after the CNPG base backups, image tracked by Renovate or
Dependabot; secrets in SOPS; a Healthchecks check. Source bucket access
through a read-only user with `s3:GetObject`/`s3:ListBucket` only.
