//! `siphon backup`: bucket listing -> pxar -> PBS snapshot.

use std::collections::HashSet;
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail, format_err};
use aws_sdk_s3::Client;
use futures::TryStreamExt;
use futures::future::LocalBoxFuture;
use openssl::hash::{Hasher, MessageDigest};
use pbs_api_types::{BackupDir, CATALOG_NAME, CryptMode, MANIFEST_BLOB_NAME};
use pbs_client::{BackupWriter, BackupWriterOptions, ChunkStream, UploadOptions};
use pbs_datastore::catalog::{BackupCatalogWriter, CatalogWriter};
use pbs_datastore::manifest::BackupManifest;
use proxmox_human_byte::HumanByte;
use pxar::encoder::SeqWrite;
use pxar::encoder::aio::Encoder;
use pxar::{Metadata, PxarVariant};
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio_util::io::ReaderStream;

use crate::cli::{BackupArgs, archive_name, ns_prefix};
use crate::s3::{self, ObjectMeta, XATTR_KEY};
use crate::tree::{self, Dir, FileNode, Node};

#[derive(Default)]
struct Stats {
    files: u64,
    bytes: u64,
    vanished: u64,
}

struct Ctx<'a> {
    s3: &'a Client,
    bucket: &'a str,
    check_etag: bool,
    stats: Stats,
    /// File list for the PBS web UI's browser (`catalog.pcat1.didx`), built
    /// in memory: a few dozen bytes per entry.
    catalog: CatalogWriter<&'a mut Vec<u8>>,
}

/// Returns the summary line.
pub async fn run(args: BackupArgs) -> Result<String> {
    let start = Instant::now();
    let s3 = s3::client(&args.s3);
    let bucket = args.s3.bucket.as_str();
    let backup_id = args.backup_id.as_deref().unwrap_or(bucket);
    let archive = archive_name(args.archive.as_deref().unwrap_or(bucket))?;

    log::info!(
        "listing s3://{bucket}/{}",
        args.s3.prefix.as_deref().unwrap_or("")
    );
    let objects = s3::list(&s3, bucket, args.s3.prefix.as_deref()).await?;
    let total: u64 = objects.iter().map(|o| o.size).sum();
    log::info!("{} objects, {total} bytes", objects.len());
    let root = tree::build(objects);

    let http = args.pbs.connect()?;
    let repo = &args.pbs.repository();
    let ns = &args.pbs.ns;
    let snapshot = BackupDir::from((
        args.backup_type,
        backup_id.to_string(),
        proxmox_time::epoch_i64(),
    ));
    log::info!("starting backup {}{snapshot} on {repo}", ns_prefix(ns));

    let writer = BackupWriter::start(
        &http,
        BackupWriterOptions {
            datastore: repo.store(),
            ns,
            backup: &snapshot,
            crypt_config: None,
            debug: false,
            benchmark: false,
            no_cache: false,
        },
    )
    .await?;

    // Like proxmox-backup-client: with the previous manifest, upload_stream
    // skips chunks the previous snapshot already has.
    let previous_manifest = previous_manifest(&writer).await;
    let upload_options = UploadOptions {
        compress: true,
        previous_manifest: previous_manifest.clone(),
        ..UploadOptions::default()
    };

    // Reuse accounting. pbs-client computes the same numbers but keeps them
    // private, so we track known chunks ourselves: the previous archive's
    // plus every chunk seen so far in this run.
    let known = Arc::new(Mutex::new(HashSet::new()));
    if let Some(manifest) = &previous_manifest
        && manifest
            .files()
            .iter()
            .any(|f| f.filename == archive.as_ref())
        && let Err(err) = writer
            .download_previous_dynamic_index(&archive, manifest, known.clone())
            .await
    {
        log::warn!("can't read previous index, reuse not reported: {err:#}");
    }
    let reused = Arc::new(AtomicU64::new(0));

    // pxar encoder -> pipe -> chunker -> upload, all in this task.
    let (pipe_tx, pipe_rx) = tokio::io::duplex(1 << 20);
    let chunks = ChunkStream::new(ReaderStream::new(pipe_rx), None, None, None).inspect_ok({
        let reused = reused.clone();
        move |chunk| {
            let digest = openssl::sha::sha256(chunk);
            if !known.lock().unwrap().insert(digest) {
                reused.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
        }
    });
    let upload = writer.upload_stream(&archive, chunks, upload_options.clone(), None);

    let mut catalog = Vec::new();
    let mut ctx = Ctx {
        s3: &s3,
        bucket,
        check_etag: !args.skip_etag_check,
        stats: Stats::default(),
        catalog: CatalogWriter::new(&mut catalog)?,
    };
    // Like proxmox-backup-client: one catalog directory per archive.
    ctx.catalog
        .start_directory(&CString::new(archive.as_ref())?)?;
    let encode = encode(&mut ctx, &root, pipe_tx);

    // On error the upload is dropped without `finish`, so PBS discards the
    // incomplete snapshot.
    let (upload_stats, ()) = tokio::try_join!(upload, encode)?;

    let Ctx {
        stats,
        catalog: mut catalog_writer,
        ..
    } = ctx;
    catalog_writer.end_directory()?;
    catalog_writer.finish()?;
    drop(catalog_writer);
    let catalog_chunks = ChunkStream::new(
        futures::stream::iter([Ok::<_, anyhow::Error>(catalog)]),
        Some(512 << 10),
        None,
        None,
    );
    let catalog_stats = writer
        .upload_stream(&CATALOG_NAME, catalog_chunks, upload_options, None)
        .await?;

    let mut manifest = BackupManifest::new(snapshot.clone());
    manifest.add_file(
        &archive,
        upload_stats.size,
        upload_stats.csum,
        CryptMode::None,
    )?;
    manifest.add_file(
        &CATALOG_NAME,
        catalog_stats.size,
        catalog_stats.csum,
        CryptMode::None,
    )?;
    let manifest = manifest.to_string(None)?;
    writer
        .upload_blob_from_data(
            manifest.into_bytes(),
            MANIFEST_BLOB_NAME.as_ref(),
            UploadOptions {
                compress: true,
                ..UploadOptions::default()
            },
        )
        .await?;
    writer.finish().await?;

    let Stats {
        files,
        bytes,
        vanished,
    } = stats;
    let reused = reused.load(Ordering::Relaxed);
    let summary = format!(
        "backup {}{snapshot} done: {files} files, {}, archive {}, reused {} ({:.1}%), \
         {vanished} vanished, {:.1}s",
        ns_prefix(ns),
        HumanByte::from(bytes),
        HumanByte::from(upload_stats.size),
        HumanByte::from(reused),
        percent(reused, upload_stats.size),
        start.elapsed().as_secs_f64()
    );
    log::info!("{summary}");
    Ok(summary)
}

/// Manifest of the previous snapshot in the group, if there is a usable one.
async fn previous_manifest(writer: &BackupWriter) -> Option<Arc<BackupManifest>> {
    match writer.previous_backup_time().await {
        Ok(Some(_)) => {}
        Ok(None) => return None,
        Err(err) => {
            log::warn!("can't get previous backup time: {err:#}");
            return None;
        }
    }
    let manifest = writer
        .download_previous_manifest()
        .await
        .and_then(|m| m.check_fingerprint(None).map(|()| m));
    match manifest {
        Ok(manifest) => Some(Arc::new(manifest)),
        Err(err) => {
            log::warn!("can't use previous manifest, uploading everything: {err:#}");
            None
        }
    }
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn dir_metadata(dir: &Dir) -> Metadata {
    Metadata::dir_builder(0o755)
        .mtime(dir.mtime().unwrap_or(SystemTime::UNIX_EPOCH))
        .build()
}

async fn encode<W: AsyncWrite + Unpin>(ctx: &mut Ctx<'_>, root: &Dir, output: W) -> Result<()> {
    let mut encoder =
        Encoder::from_tokio(PxarVariant::Unified(output), &dir_metadata(root), None).await?;
    encode_dir(&mut encoder, ctx, root).await?;
    encoder.finish().await?;
    encoder.close().await?;
    Ok(())
}

/// Encode the children of `dir` into the currently open directory.
fn encode_dir<'a, 'e, T: SeqWrite + 'e>(
    encoder: &'a mut Encoder<'e, T>,
    ctx: &'a mut Ctx<'_>,
    dir: &'a Dir,
) -> LocalBoxFuture<'a, Result<()>> {
    Box::pin(async move {
        for (name, node) in &dir.children {
            match node {
                Node::Dir(child) => {
                    encoder.create_directory(name, &dir_metadata(child)).await?;
                    ctx.catalog.start_directory(&CString::new(name.as_str())?)?;
                    encode_dir(encoder, ctx, child).await?;
                    encoder.finish().await?;
                    ctx.catalog.end_directory()?;
                }
                Node::File(file) => encode_file(encoder, ctx, name, file)
                    .await
                    .with_context(|| format!("object {}", file.object.key))?,
            }
        }
        Ok(())
    })
}

async fn encode_file<T: SeqWrite>(
    encoder: &mut Encoder<'_, T>,
    ctx: &mut Ctx<'_>,
    name: &str,
    file: &FileNode,
) -> Result<()> {
    let key = &file.object.key;
    let resp = ctx.s3.get_object().bucket(ctx.bucket).key(key).send().await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
            log::warn!("{key}: deleted since listing, skipped");
            ctx.stats.vanished += 1;
            return Ok(());
        }
        Err(err) => return Err(aws_sdk_s3::Error::from(err).into()),
    };

    let meta = ObjectMeta::from_get(&resp);
    let size: u64 = resp
        .content_length()
        .ok_or_else(|| format_err!("no Content-Length"))?
        .try_into()?;
    let mtime = resp
        .last_modified()
        .map(|t| SystemTime::try_from(*t))
        .transpose()?
        .unwrap_or(file.object.last_modified);

    let mut builder = Metadata::file_builder(0o644).mtime(mtime);
    for (name, value) in meta.to_xattrs() {
        builder = builder.xattr(name, value);
    }
    if file.escaped {
        builder = builder.xattr(XATTR_KEY, key);
    }

    let mut out = encoder.create_file(&builder.build(), name, size).await?;
    let mut body = resp.body.into_async_read();
    let mut md5 = Hasher::new(MessageDigest::md5())?;
    let mut buf = vec![0u8; 256 << 10];
    let mut written = 0u64;
    loop {
        let n = body.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if written + n as u64 > size {
            bail!("body longer than Content-Length {size}");
        }
        md5.update(&buf[..n])?;
        out.write_all(&buf[..n]).await?;
        written += n as u64;
    }
    if written != size {
        bail!("body ended after {written} of {size} bytes");
    }

    if let Some(expected) = meta.etag_md5() {
        let actual = hex::encode(md5.finish()?);
        if actual != expected {
            if ctx.check_etag {
                bail!("MD5 {actual} doesn't match ETag {expected}");
            }
            log::warn!("{key}: MD5 {actual} doesn't match ETag {expected}");
        }
    }

    let mtime_secs = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    ctx.catalog
        .add_file(&CString::new(name)?, size, mtime_secs)?;
    ctx.stats.files += 1;
    ctx.stats.bytes += size;
    log::debug!("{key}: {size} bytes");
    Ok(())
}
