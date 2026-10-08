# siphon

Back up an S3 bucket into Proxmox Backup Server.

siphon lists a bucket, streams every object into a pxar archive and uploads it
as a PBS snapshot using the same client crates as `proxmox-backup-client`.
Nothing is staged on local disk, so the bucket isn't copied twice. Status:
early development; see [docs/DESIGN.md](https://github.com/vooon/siphon/blob/main/docs/DESIGN.md).

## Container image

```
ghcr.io/vooon/siphon:<version>
```

Built by GitHub Actions from `main` and from `v*` tags.

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
