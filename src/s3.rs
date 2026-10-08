//! S3 access: client setup, bucket listing, object metadata <-> xattrs.

use std::collections::BTreeMap;
use std::time::SystemTime;

use anyhow::{Context, Result, format_err};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::operation::get_object::GetObjectOutput;

use crate::cli::S3Args;
use crate::tree::Object;

/// Prefix of all xattrs siphon writes.
pub const XATTR_PREFIX: &str = "user.s3.";
/// xattr holding the original key of an escaped entry.
pub const XATTR_KEY: &str = "user.s3.key";
const XATTR_USER_META: &str = "user.s3.meta.";

pub fn client(args: &S3Args) -> Client {
    let credentials = Credentials::new(
        &args.access_key_id,
        &args.secret_access_key,
        None,
        None,
        "siphon",
    );
    let mut config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(args.region.clone()))
        .credentials_provider(credentials)
        .force_path_style(args.path_style)
        // S3-compatible stores (RGW, rustfs, ...) don't all support the
        // newer default checksum trailers.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    if let Some(endpoint) = &args.endpoint {
        config = config.endpoint_url(endpoint);
    }
    Client::from_conf(config.build())
}

/// List all objects of `bucket` under `prefix`, in key order.
pub async fn list(client: &Client, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>> {
    let mut objects = Vec::new();
    let mut pages = client
        .list_objects_v2()
        .bucket(bucket)
        .set_prefix(prefix.map(str::to_string))
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.with_context(|| format!("listing bucket {bucket}"))?;
        for o in page.contents() {
            let key = o
                .key()
                .ok_or_else(|| format_err!("listing returned an object without key"))?;
            let last_modified = o
                .last_modified()
                .map(|t| SystemTime::try_from(*t))
                .transpose()
                .with_context(|| format!("bad LastModified for {key}"))?
                .unwrap_or(SystemTime::UNIX_EPOCH);
            objects.push(Object {
                key: key.to_string(),
                size: o.size().unwrap_or(0).try_into().unwrap_or(0),
                last_modified,
            });
        }
    }
    Ok(objects)
}

/// The object metadata siphon keeps, as returned by `GetObject`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectMeta {
    pub etag: Option<String>,
    pub version_id: Option<String>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub content_disposition: Option<String>,
    pub content_language: Option<String>,
    pub cache_control: Option<String>,
    /// `x-amz-meta-*`, names without the prefix.
    pub user: BTreeMap<String, String>,
}

impl ObjectMeta {
    pub fn from_get(out: &GetObjectOutput) -> Self {
        let s = |v: Option<&str>| v.map(str::to_string);
        Self {
            etag: s(out.e_tag()),
            version_id: s(out.version_id()).filter(|v| v != "null"),
            content_type: s(out.content_type()),
            content_encoding: s(out.content_encoding()),
            content_disposition: s(out.content_disposition()),
            content_language: s(out.content_language()),
            cache_control: s(out.cache_control()),
            user: out
                .metadata()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        }
    }

    fn fields(&self) -> [(&'static str, &Option<String>); 7] {
        [
            ("etag", &self.etag),
            ("version-id", &self.version_id),
            ("content-type", &self.content_type),
            ("content-encoding", &self.content_encoding),
            ("content-disposition", &self.content_disposition),
            ("content-language", &self.content_language),
            ("cache-control", &self.cache_control),
        ]
    }

    /// xattrs for this object, sorted by name.
    pub fn to_xattrs(&self) -> Vec<(String, String)> {
        let mut xattrs: Vec<(String, String)> = self
            .fields()
            .into_iter()
            .filter_map(|(name, value)| {
                value
                    .as_ref()
                    .map(|v| (format!("{XATTR_PREFIX}{name}"), v.clone()))
            })
            .chain(
                self.user
                    .iter()
                    .map(|(k, v)| (format!("{XATTR_USER_META}{k}"), v.clone())),
            )
            .collect();
        xattrs.sort();
        xattrs
    }

    /// Parse xattrs written by [`to_xattrs`](Self::to_xattrs). Unknown names
    /// (and [`XATTR_KEY`]) are ignored.
    pub fn from_xattrs<'a>(xattrs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut meta = Self::default();
        for (name, value) in xattrs {
            let Some(name) = name.strip_prefix(XATTR_PREFIX) else {
                continue;
            };
            if let Some(k) = name.strip_prefix("meta.") {
                meta.user.insert(k.to_string(), value.to_string());
                continue;
            }
            let value = Some(value.to_string());
            match name {
                "etag" => meta.etag = value,
                "version-id" => meta.version_id = value,
                "content-type" => meta.content_type = value,
                "content-encoding" => meta.content_encoding = value,
                "content-disposition" => meta.content_disposition = value,
                "content-language" => meta.content_language = value,
                "cache-control" => meta.cache_control = value,
                _ => {}
            }
        }
        meta
    }

    /// The ETag is the MD5 of the body for single-part uploads without
    /// SSE-KMS/SSE-C; multipart ETags contain a `-`.
    pub fn etag_md5(&self) -> Option<String> {
        let etag = self.etag.as_deref()?.trim_matches('"');
        (etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| etag.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xattr_roundtrip() {
        let meta = ObjectMeta {
            etag: Some("\"0123\"".into()),
            version_id: Some("v1".into()),
            content_type: Some("application/octet-stream".into()),
            cache_control: Some("no-cache".into()),
            user: [("barman".to_string(), "wal".to_string())].into(),
            ..Default::default()
        };
        let xattrs = meta.to_xattrs();
        assert!(xattrs.is_sorted());
        assert!(xattrs.contains(&("user.s3.meta.barman".into(), "wal".into())));
        let parsed = ObjectMeta::from_xattrs(
            xattrs
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .chain([(XATTR_KEY, "a//b"), ("user.other", "x")]),
        );
        assert_eq!(parsed, meta);
    }

    #[test]
    fn etag_md5() {
        let m = |e: &str| ObjectMeta {
            etag: Some(e.into()),
            ..Default::default()
        };
        assert_eq!(
            m("\"D41D8CD98F00B204E9800998ECF8427E\"")
                .etag_md5()
                .as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(m("\"d41d8cd98f00b204e9800998ecf8427e-2\"").etag_md5(), None);
    }
}
