# siphon

Back up an S3 bucket into Proxmox Backup Server.

siphon lists a bucket, streams every object into a pxar archive and uploads it
as a PBS snapshot using the same client crates as `proxmox-backup-client`.
Nothing is staged on local disk, so the bucket isn't copied twice. Status:
early development; see [docs/DESIGN.md](https://github.com/vooon/siphon/blob/main/docs/DESIGN.md).

## Usage

Every option is also an environment variable; see `siphon backup --help`.

```
export PBS_REPOSITORY='backup@pbs!siphon@pbs.example.com:store'
export PBS_PASSWORD=...  PBS_FINGERPRINT=...
export S3_ENDPOINT=https://s3.example.com S3_BUCKET=my-bucket
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...

siphon backup                                   # -> host/my-bucket/<now>, my-bucket.pxar
siphon restore --snapshot host/my-bucket \
               --s3-bucket my-bucket-restored   # latest snapshot -> bucket
```

Set `HC_PING_URL` to report runs to [Healthchecks](https://healthchecks.io):
start, then success with a summary or failure with the error.

Object metadata (Content-Type and friends, `x-amz-meta-*`, ETag, version ID)
is kept as `user.s3.*` xattrs in the archive and put back on restore.

## Container image

```
ghcr.io/vooon/siphon:<version>
```

Built by GitHub Actions from `main` and from `v*` tags, after an end-to-end
test against rustfs and PBS (`e2e/run.sh`). Releases with a linux-amd64
binary are on the GitHub releases page.

## Building

```
cargo build --release
```

Needs the native libraries that `pbs-client` links against (Debian names):
`libacl1-dev libcrypt-dev libssl-dev libsystemd-dev libzstd-dev uuid-dev
libclang-dev pkg-config`. Or just `docker build .`.

## License

AGPL-3.0, same as `proxmox-backup`, whose crates siphon links. See
[LICENSE](https://github.com/vooon/siphon/blob/main/LICENSE).

## Disclaimer

This project is not affiliated in any way with Proxmox Server Solutions GmbH.
"Proxmox", the Proxmox logo and related names are the property of their
respective owners; here they are used only to state compatibility. See
[proxmox.com](https://www.proxmox.com) for their products.
