//! `siphon restore`: PBS snapshot -> pxar -> objects in a bucket.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail, format_err};
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures::future::LocalBoxFuture;
use pbs_api_types::{BackupDir, BackupNamespace, BackupPart, MANIFEST_BLOB_NAME, SnapshotListItem};
use pbs_client::{BackupReader, HttpClient, RemoteChunkReader};
use pbs_datastore::dynamic_index::{BufferedDynamicReader, LocalDynamicReadAt};
use pbs_datastore::index::IndexFile;
use pxar::EntryKind;
use pxar::accessor::ReadAt;
use pxar::accessor::aio::{Accessor, Directory, FileEntry};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::cli::{RestoreArgs, archive_name, ns_prefix};
use crate::s3::{self, ObjectMeta, XATTR_KEY};
use crate::tree::RESERVED_DIR;

type Reader = Arc<dyn ReadAt + Send + Sync>;

struct Ctx<'a> {
    s3: &'a Client,
    args: &'a RestoreArgs,
    files: u64,
    bytes: u64,
}

pub async fn run(args: RestoreArgs) -> Result<()> {
    let start = Instant::now();
    let http = args.pbs.connect()?;
    let repo = &args.pbs.repository();
    let ns = &args.pbs.ns;
    let snapshot = resolve_snapshot(&http, repo.store(), ns, &args.snapshot).await?;
    let archive = archive_name(args.archive.as_deref().unwrap_or(&snapshot.group.id))?;
    log::info!(
        "restoring {}{snapshot}/{archive} from {repo}",
        ns_prefix(ns)
    );

    let reader = BackupReader::start(&http, None, repo.store(), ns, &snapshot, false).await?;
    let (manifest, _) = reader.download_manifest().await?;
    manifest.check_fingerprint(None)?;
    let index = reader.download_dynamic_index(&manifest, &archive).await?;
    let most_used = index.find_most_used_chunks(8);
    let file_info = manifest.lookup_file_info(&archive)?;
    let chunk_reader = RemoteChunkReader::new(
        reader.clone(),
        None,
        file_info.chunk_crypt_mode(),
        most_used,
    );
    let buffered = BufferedDynamicReader::new(index, chunk_reader);
    let size = buffered.archive_size();
    let read_at: Reader = Arc::new(LocalDynamicReadAt::new(buffered));
    let accessor = Accessor::new(pxar::PxarVariant::Unified(read_at), size).await?;

    let s3 = s3::client(&args.s3);
    let bucket = &args.s3.bucket;
    if !args.overwrite && !args.dry_run {
        let existing = s3
            .list_objects_v2()
            .bucket(bucket)
            .set_prefix(args.s3.prefix.clone())
            .max_keys(1)
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .with_context(|| format!("listing bucket {bucket}"))?;
        if existing.key_count().unwrap_or(0) > 0 {
            bail!("bucket {bucket} is not empty (under the prefix); use --overwrite");
        }
    }

    let mut ctx = Ctx {
        s3: &s3,
        args: &args,
        files: 0,
        bytes: 0,
    };
    let root = accessor.open_root().await?;
    walk(&root, String::new(), &mut ctx).await?;

    log::info!(
        "restore done: {} objects, {} bytes{}, {:.1}s",
        ctx.files,
        ctx.bytes,
        if args.dry_run { " (dry run)" } else { "" },
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `type/id/time` as is; `type/id` resolves to the latest finished snapshot.
async fn resolve_snapshot(
    http: &HttpClient,
    store: &str,
    ns: &BackupNamespace,
    path: &str,
) -> Result<BackupDir> {
    let group = match path.parse::<BackupPart>()? {
        BackupPart::Dir(dir) => return Ok(dir),
        BackupPart::Group(group) => group,
    };
    let mut args = serde_json::to_value(&group)?;
    if !ns.is_root() {
        args["ns"] = json!(ns);
    }
    let mut resp = http
        .get(
            &format!("api2/json/admin/datastore/{store}/snapshots"),
            Some(args),
        )
        .await?;
    let list: Vec<SnapshotListItem> = serde_json::from_value(resp["data"].take())?;
    list.into_iter()
        .filter(|s| {
            s.files
                .iter()
                .any(|f| f.filename == MANIFEST_BLOB_NAME.as_ref())
        })
        .max_by_key(|s| s.backup.time)
        .map(|s| s.backup)
        .ok_or_else(|| format_err!("group {group} has no finished snapshots"))
}

fn walk<'a>(
    dir: &'a Directory<Reader>,
    path: String,
    ctx: &'a mut Ctx<'_>,
) -> LocalBoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let mut entries = dir.read_dir();
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| format_err!("non-UTF-8 name in {path:?}"))?;
            let child = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}/{name}")
            };
            let file = entry.decode_entry().await?;
            match file.kind() {
                EntryKind::Directory => {
                    walk(&file.enter_directory().await?, child, ctx).await?;
                }
                EntryKind::File { size, .. } => {
                    let size = *size;
                    restore_file(&file, &child, size, ctx)
                        .await
                        .with_context(|| format!("restoring {child}"))?;
                }
                _ => log::warn!("{child}: not a regular file, skipped"),
            }
        }
        Ok(())
    })
}

async fn restore_file(
    file: &FileEntry<Reader>,
    path: &str,
    size: u64,
    ctx: &mut Ctx<'_>,
) -> Result<()> {
    let xattrs: Vec<(String, String)> = file
        .metadata()
        .xattrs
        .iter()
        .map(|x| {
            (
                x.name().to_string_lossy().into_owned(),
                String::from_utf8_lossy(x.value()).into_owned(),
            )
        })
        .collect();
    let key = match xattrs.iter().find(|(n, _)| n == XATTR_KEY) {
        Some((_, key)) => key.clone(),
        None if path.starts_with(&format!("{RESERVED_DIR}/")) => {
            log::warn!("{path}: no {XATTR_KEY} xattr, skipped");
            return Ok(());
        }
        None => path.to_string(),
    };
    if let Some(prefix) = &ctx.args.s3.prefix
        && !key.starts_with(prefix.as_str())
    {
        return Ok(());
    }
    let meta = ObjectMeta::from_xattrs(xattrs.iter().map(|(n, v)| (n.as_str(), v.as_str())));

    ctx.files += 1;
    ctx.bytes += size;
    if ctx.args.dry_run {
        println!("{key}\t{size}");
        return Ok(());
    }

    let contents = file.contents().await?;
    if size <= ctx.args.part_size as u64 {
        put_object(ctx, &key, &meta, contents, size).await
    } else {
        multipart_upload(ctx, &key, &meta, contents).await
    }
}

/// Set the metadata fields shared by PutObject and CreateMultipartUpload.
macro_rules! with_meta {
    ($req:expr, $meta:expr) => {{
        let meta: &ObjectMeta = $meta;
        $req.set_content_type(meta.content_type.clone())
            .set_content_encoding(meta.content_encoding.clone())
            .set_content_disposition(meta.content_disposition.clone())
            .set_content_language(meta.content_language.clone())
            .set_cache_control(meta.cache_control.clone())
            .set_metadata(
                (!meta.user.is_empty())
                    .then(|| meta.user.clone().into_iter().collect::<HashMap<_, _>>()),
            )
    }};
}

async fn put_object(
    ctx: &Ctx<'_>,
    key: &str,
    meta: &ObjectMeta,
    mut contents: impl AsyncRead + Unpin,
    size: u64,
) -> Result<()> {
    let mut data = Vec::with_capacity(size as usize);
    contents.read_to_end(&mut data).await?;
    if data.len() as u64 != size {
        bail!("archive entry has {} bytes, expected {size}", data.len());
    }
    let req = ctx
        .s3
        .put_object()
        .bucket(&ctx.args.s3.bucket)
        .key(key)
        .body(ByteStream::from(data));
    let out = with_meta!(req, meta)
        .send()
        .await
        .map_err(aws_sdk_s3::Error::from)?;

    // Single-part upload: the new ETag is the MD5 of what we sent.
    if let (Some(expected), Some(etag)) = (meta.etag_md5(), out.e_tag())
        && etag.trim_matches('"').to_ascii_lowercase() != expected
    {
        log::warn!("{key}: ETag {etag} differs from original {expected}");
    }
    log::debug!("{key}: {size} bytes");
    Ok(())
}

async fn multipart_upload(
    ctx: &Ctx<'_>,
    key: &str,
    meta: &ObjectMeta,
    mut contents: impl AsyncRead + Unpin,
) -> Result<()> {
    let bucket = &ctx.args.s3.bucket;
    let req = ctx.s3.create_multipart_upload().bucket(bucket).key(key);
    let upload = with_meta!(req, meta)
        .send()
        .await
        .map_err(aws_sdk_s3::Error::from)?;
    let upload_id = upload
        .upload_id()
        .ok_or_else(|| format_err!("no upload id"))?;

    let result: Result<()> = async {
        let mut parts = Vec::new();
        loop {
            let mut part = Vec::with_capacity(ctx.args.part_size);
            (&mut contents)
                .take(ctx.args.part_size as u64)
                .read_to_end(&mut part)
                .await?;
            if part.is_empty() && !parts.is_empty() {
                break;
            }
            let last = part.len() < ctx.args.part_size;
            let number = parts.len() as i32 + 1;
            let out = ctx
                .s3
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(number)
                .body(ByteStream::from(part))
                .send()
                .await
                .map_err(aws_sdk_s3::Error::from)?;
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(out.e_tag().map(str::to_string))
                    .build(),
            );
            if last {
                break;
            }
        }
        log::debug!("{key}: {} parts", parts.len());
        ctx.s3
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = ctx
            .s3
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
    }
    result
}
