use chv_errors::ChvError;
use object_store::ObjectStoreExt;
use tracing::{info, warn};

/// Abstraction over backup artifact shipping targets.
#[async_trait::async_trait]
pub trait BackupShipper: Send + Sync {
    /// Delete a remote artifact.
    async fn delete(&self, remote_path: &str) -> Result<(), ChvError>;
}

// ── Null Shipper (dev/testing) ─────────────────────────────────────────────

pub struct NullShipper;

#[async_trait::async_trait]
impl BackupShipper for NullShipper {
    async fn delete(&self, _remote_path: &str) -> Result<(), ChvError> {
        Ok(())
    }
}

// ── NFS Shipper ────────────────────────────────────────────────────────────

pub struct NfsShipper;

impl NfsShipper {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl BackupShipper for NfsShipper {
    async fn delete(&self, remote_path: &str) -> Result<(), ChvError> {
        match tokio::fs::remove_file(remote_path).await {
            Ok(()) => {
                info!(path = %remote_path, "NFS shipper: deleted artifact");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ChvError::Internal {
                reason: format!("NFS shipper delete failed: {e}"),
            }),
        }
    }
}

// ── S3 Shipper ─────────────────────────────────────────────────────────────

/// S3-compatible backup artifact shipper (retention deletes), backed by
/// `object_store` (aws backend, sigv4-signed).
///
/// Replaces the previous `rust-s3` implementation (issue #177): rust-s3
/// 0.37 pinned `quick-xml` 0.38.4, which carries RUSTSEC-2026-0194 and
/// RUSTSEC-2026-0195 (fixed in quick-xml >= 0.41). The only S3 operation
/// CHV performs is deleting shipped backup artifacts during retention
/// cleanup, which `object_store`'s `ObjectStore::delete` covers directly.
pub struct S3Shipper {
    bucket_name: String,
    store: object_store::aws::AmazonS3,
}

impl S3Shipper {
    pub fn new(
        bucket: String,
        region: String,
        endpoint: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
    ) -> Result<Self, ChvError> {
        // Seed from the environment (AWS_* variables) so deployments that
        // relied on implicit credentials keep working, then apply explicit
        // configuration on top. Note the previous rust-s3 default chain also
        // consulted ~/.aws/credentials profiles and IMDS; only explicit
        // config keys and environment variables are supported now.
        //
        // Environment note (standard AWS SDK semantics, but a behavioral
        // delta vs rust-s3): from_env() also honors AWS_ENDPOINT /
        // AWS_ENDPOINT_URL_S3, which retarget the client when no explicit
        // endpoint is configured. Because retention cleanup issues
        // destructive DELETEs, operators should treat those variables as
        // authoritative redirection, not ambient noise — do not set them
        // host-wide for unrelated tooling on a CHV control-plane host.
        let mut builder = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(&bucket)
            .with_region(&region)
            // Preserve the previous rust-s3 wire behavior: single-object
            // `DELETE /key` requests (core S3 API, supported by every
            // S3-compatible provider) instead of the bulk `POST /?delete`
            // API, which not all providers implement.
            .with_disable_bulk_delete(true);

        if let Some(endpoint) = &endpoint {
            // Preserve the previous `Region::Custom` behavior: plain-http
            // endpoints (e.g. a local MinIO) are allowed.
            if endpoint.starts_with("http://") {
                builder = builder.with_allow_http(true);
            }
            builder = builder.with_endpoint(endpoint);
        }

        if let (Some(ak), Some(sk)) = (access_key, secret_key) {
            builder = builder.with_access_key_id(ak).with_secret_access_key(sk);
        }

        let store = builder.build().map_err(|e| ChvError::Internal {
            reason: format!("failed to create S3 client: {e}"),
        })?;

        Ok(Self {
            bucket_name: bucket,
            store,
        })
    }
}

#[async_trait::async_trait]
impl BackupShipper for S3Shipper {
    async fn delete(&self, remote_path: &str) -> Result<(), ChvError> {
        // object_store paths are '/'-delimited segments without a leading
        // separator; empty segments (leading/trailing '/') are skipped.
        let path = object_store::path::Path::from(remote_path);
        match self.store.delete(&path).await {
            Ok(()) => {
                info!(
                    key = %remote_path,
                    bucket = %self.bucket_name,
                    "S3 shipper: deleted artifact"
                );
                Ok(())
            }
            Err(e) => {
                warn!(key = %remote_path, error = %e, "S3 shipper: delete failed");
                Err(ChvError::Internal {
                    reason: format!("S3 delete failed: {e}"),
                })
            }
        }
    }
}

/// ASCII characters that the `http` URI parser (used internally by
/// `object_store`) rejects. `url::Url::parse` accepts most of them by
/// percent-encoding, so they must be rejected explicitly: the raw endpoint
/// string is what reaches `object_store`, not the normalized URL.
const URI_ILLEGAL_CHARS: &[char] = &['"', '<', '>', '\\', '^', '`', '{', '|', '}'];

/// Validate an operator-supplied S3 endpoint before it reaches `object_store`.
///
/// `object_store` 0.14.2 feeds the endpoint into its HTTP request builder,
/// where a malformed value (e.g. one containing a space) can panic inside the
/// `http`/`url` parsers (`InvalidUriChar`) instead of returning an error
/// (#677). A typo in a backup destination must surface as a normal
/// configuration error so retention keeps the row for an operator to fix.
fn validate_s3_endpoint(endpoint: &str) -> Result<(), ChvError> {
    let reject = |detail: String| ChvError::InvalidArgument {
        field: "destination".to_string(),
        reason: format!("invalid S3 endpoint '{endpoint}': {detail}"),
    };

    if let Some(c) = endpoint.chars().find(|c| {
        c.is_control() || c.is_whitespace() || !c.is_ascii() || URI_ILLEGAL_CHARS.contains(c)
    }) {
        return Err(reject(format!(
            "contains character {c:?}, which is not allowed in a URL"
        )));
    }

    let parsed =
        url::Url::parse(endpoint).map_err(|e| reject(format!("not an absolute URL: {e}")))?;

    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(reject(format!(
                "unsupported scheme '{scheme}': expected http or https"
            )))
        }
    }

    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(reject("missing host".to_string()));
    }

    Ok(())
}

/// Convenience constructor that builds a shipper from a destination string.
///
/// Supported formats:
/// - `s3://bucket/prefix?region=us-east-1&endpoint=http://localhost:9000`
/// - `nfs:///mnt/backups`
/// - `null`
pub fn shipper_from_destination(
    destination: &str,
    access_key: Option<String>,
    secret_key: Option<String>,
) -> Result<Box<dyn BackupShipper>, ChvError> {
    if destination.eq_ignore_ascii_case("null") {
        return Ok(Box::new(NullShipper));
    }

    if destination.starts_with("nfs://") || destination.starts_with("nfs+") {
        return Ok(Box::new(NfsShipper::new()));
    }

    if let Some(rest) = destination.strip_prefix("s3://") {
        let mut parts = rest.splitn(2, '/');
        let bucket = parts.next().unwrap_or("").to_string();
        let remainder = parts.next().unwrap_or("");

        let (_prefix, query) = if let Some(qidx) = remainder.find('?') {
            (&remainder[..qidx], &remainder[qidx + 1..])
        } else {
            (remainder, "")
        };

        let mut region = "us-east-1".to_string();
        let mut endpoint = None;
        for pair in query.split('&') {
            let mut kv = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                match k {
                    "region" => region = v.to_string(),
                    "endpoint" => endpoint = Some(v.to_string()),
                    _ => {}
                }
            }
        }

        if let Some(endpoint) = endpoint.as_deref() {
            // Validate before object_store sees the value: malformed
            // endpoints (including an empty value) panic in 0.14.2 instead
            // of returning an error (#677).
            validate_s3_endpoint(endpoint)?;
        }

        return Ok(Box::new(S3Shipper::new(
            bucket, region, endpoint, access_key, secret_key,
        )?));
    }

    Err(ChvError::InvalidArgument {
        field: "destination".to_string(),
        reason: format!(
            "unsupported backup destination '{}': expected s3://, nfs://, or null",
            destination
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_nfs_shipper_delete_handles_not_found() {
        let dst_dir = tempfile::tempdir().unwrap();
        let shipper = NfsShipper::new();

        let missing = dst_dir.path().join("does-not-exist.backup");
        // Should not error when file is already gone
        shipper.delete(missing.to_str().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn test_nfs_shipper_delete_removes_file() {
        let dst_dir = tempfile::tempdir().unwrap();
        let shipper = NfsShipper::new();

        let file = dst_dir.path().join("to-delete.backup");
        std::fs::write(&file, b"x").unwrap();
        assert!(file.exists());

        shipper.delete(file.to_str().unwrap()).await.unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn test_shipper_from_destination_null() {
        let shipper = shipper_from_destination("null", None, None).unwrap();
        // Type erasure means we can only verify it doesn't panic and implements BackupShipper.
        let _ = shipper; // compilation check
    }

    #[test]
    fn test_shipper_from_destination_nfs() {
        let shipper = shipper_from_destination("nfs:///mnt/backups", None, None).unwrap();
        let _ = shipper;
    }

    #[test]
    fn test_shipper_from_destination_s3_parses_bucket_and_prefix() {
        // We can't actually construct S3Shipper without valid credentials/network,
        // but we can verify the parser by checking it returns Ok for well-formed URLs.
        let result = shipper_from_destination(
            "s3://my-bucket/backups?region=us-west-2&endpoint=http://localhost:9000",
            Some("ak".into()),
            Some("sk".into()),
        );
        assert!(
            result.is_ok(),
            "S3 shipper construction failed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_shipper_from_destination_rejects_invalid() {
        let result = shipper_from_destination("ftp://host/path", None, None);
        assert!(matches!(result, Err(ChvError::InvalidArgument { .. })));
    }

    #[test]
    fn test_shipper_from_destination_rejects_malformed_s3_endpoint() {
        // Regression for #677: these previously reached object_store, which
        // panicked (InvalidUriChar) instead of returning an error.
        for endpoint in [
            "",
            "not a url",
            "http://",
            "ftp://host",
            "http://127.0.0.1:9000/backups dir",
        ] {
            let destination = format!("s3://bucket/prefix?endpoint={endpoint}");
            match shipper_from_destination(
                &destination,
                Some("ak".to_string()),
                Some("sk".to_string()),
            ) {
                Err(ChvError::InvalidArgument { field, reason }) => {
                    assert_eq!(field, "destination");
                    assert!(
                        reason.contains(&format!("'{endpoint}'")),
                        "reason must name the offending endpoint {endpoint:?}: {reason}"
                    );
                }
                other => panic!(
                    "expected InvalidArgument for endpoint {endpoint:?}, got {:?}",
                    other.map(|_| "Ok(shipper)")
                ),
            }
        }
    }

    #[test]
    fn test_shipper_from_destination_accepts_http_and_https_endpoints() {
        // Construction is local (no network call); only the parsed config is
        // handed to object_store.
        for endpoint in ["http://127.0.0.1:9000", "https://s3.example.com"] {
            let destination = format!("s3://bucket/prefix?endpoint={endpoint}");
            let result = shipper_from_destination(
                &destination,
                Some("ak".to_string()),
                Some("sk".to_string()),
            );
            assert!(
                result.is_ok(),
                "valid endpoint {endpoint:?} was rejected: {:?}",
                result.err()
            );
        }
    }
}
