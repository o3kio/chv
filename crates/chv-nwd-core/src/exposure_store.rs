//! Durable journal for applied service exposures (DNAT + forward-accept).
//!
//! `chv-nwd` tracks service exposures in memory so it can re-assert each
//! exposure's `forward` accept rule after a firewall apply rebuilds the base
//! chain. nftables rules survive a process restart but not a host reboot, and
//! the in-memory record is lost either way. Without a durable copy, a daemon
//! restart or node reboot silently removes published ports/NAT for **running**
//! VMs — the guest stays up but becomes unreachable from outside.
//!
//! This journal stores the applied exposures per network under
//! `/var/lib/chv/nwd/exposures/<network_id>.json`. The production daemon
//! replays it before serving, re-installing each exposure idempotently.
//!
//! Persistence is desired-state: an exposure is journaled only after its nft
//! rules were installed successfully. A corrupt entry fails daemon startup
//! loudly, matching the topology/policy journals: silently skipping a corrupt
//! exposure would reintroduce the silent-loss class this file exists to close.
//! A record that decodes but fails replay re-validation is different: it is
//! skipped loudly (`chv_nwd_exposure_replay_skipped_total{reason="invalid_record"}`)
//! and retained on disk for inspection/retry, without aborting startup.

use chv_errors::ChvError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Persistent exposure journal root. Matches the `ExecStartPre=` directory
/// declared in both `packaging/systemd/chv-nwd.service` and
/// `docs/examples/systemd/chv-nwd.service`.
pub const DEFAULT_EXPOSURE_STATE_DIR: &str = "/var/lib/chv/nwd/exposures";

const EXTENSION: &str = "json";

/// Monotonic suffix for the temporary sibling of an atomic write (same
/// collision-free scheme as the topology/policy journals).
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One applied service exposure. The fields mirror the executor's internal
/// `ExposureSpec`; `safe_exposure_id` is the sanitized id used in nft comments.
///
/// Every field is required on decode. A journal entry is written only after the
/// full spec validated, so a parseable-but-truncated object (missing `protocol`
/// or both ports) means the file was damaged. `serde(default)` would turn that
/// into a silent replay of `dport 0`/an empty IP; failing the parse instead
/// aborts recovery loudly like every other corrupt journal entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredExposure {
    pub safe_exposure_id: String,
    pub protocol: String,
    pub external_port: u32,
    pub target_ip: String,
    pub target_port: u32,
}

/// All exposures currently applied for one network.
///
/// The container field is required too: an empty set is never persisted (it
/// removes the file), so a journal file with no `exposures` key is corrupt, not
/// an empty journal. `Default` remains for callers constructing an empty set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredExposures {
    pub exposures: Vec<StoredExposure>,
}

impl StoredExposures {
    pub fn is_empty(&self) -> bool {
        self.exposures.is_empty()
    }
}

/// Filesystem-backed journal of applied service exposures.
#[derive(Debug, Clone)]
pub struct ExposureStore {
    dir: PathBuf,
}

impl ExposureStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Journal root used by the production daemon.
    pub fn default_dir() -> Self {
        Self::new(DEFAULT_EXPOSURE_STATE_DIR)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, network_id: &str) -> PathBuf {
        self.dir.join(format!("{network_id}.{EXTENSION}"))
    }

    async fn ensure_dir(&self) -> Result<(), ChvError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| ChvError::Io {
                path: self.dir.to_string_lossy().to_string(),
                source: e,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).await;
        }
        Ok(())
    }

    /// Atomically persist one network's applied exposures. An empty set removes
    /// the entry, so a withdrawn last exposure leaves no stale file.
    pub async fn persist(
        &self,
        network_id: &str,
        exposures: &StoredExposures,
    ) -> Result<(), ChvError> {
        crate::topology_store::validate_network_id(network_id)?;
        if exposures.is_empty() {
            return self.remove(network_id).await;
        }
        self.ensure_dir().await?;

        let bytes = serde_json::to_vec(exposures).map_err(|e| ChvError::Internal {
            reason: format!("failed to encode exposure journal entry for {network_id}: {e}"),
        })?;

        let target = self.path_for(network_id);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            network_id,
            std::process::id(),
            sequence
        ));

        let write_result = async {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&temp)
                .await
                .map_err(|e| ChvError::Io {
                    path: temp.to_string_lossy().to_string(),
                    source: e,
                })?;
            file.write_all(&bytes).await.map_err(|e| ChvError::Io {
                path: temp.to_string_lossy().to_string(),
                source: e,
            })?;
            file.sync_all().await.map_err(|e| ChvError::Io {
                path: temp.to_string_lossy().to_string(),
                source: e,
            })?;
            drop(file);
            tokio::fs::rename(&temp, &target)
                .await
                .map_err(|e| ChvError::Io {
                    path: target.to_string_lossy().to_string(),
                    source: e,
                })?;
            self.sync_dir().await
        }
        .await;

        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        write_result
    }

    /// Read one journal entry, if present.
    pub async fn load(&self, network_id: &str) -> Result<Option<StoredExposures>, ChvError> {
        crate::topology_store::validate_network_id(network_id)?;
        let path = self.path_for(network_id);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ChvError::Io {
                    path: path.to_string_lossy().to_string(),
                    source: e,
                })
            }
        };
        let exposures = serde_json::from_slice(&bytes).map_err(|e| ChvError::Internal {
            reason: format!("corrupt exposure journal entry {}: {e}", path.display()),
        })?;
        Ok(Some(exposures))
    }

    /// Load every journal entry, sorted by network id for deterministic replay.
    pub async fn load_all(&self) -> Result<Vec<(String, StoredExposures)>, ChvError> {
        let mut read_dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(read_dir) => read_dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(ChvError::Io {
                    path: self.dir.to_string_lossy().to_string(),
                    source: e,
                })
            }
        };

        let mut entries = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| ChvError::Io {
            path: self.dir.to_string_lossy().to_string(),
            source: e,
        })? {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // Ignore the atomic-write temporaries and anything that is not a
            // journal entry; a stray non-json file must not be parsed.
            if file_name.starts_with('.')
                || path.extension().and_then(|e| e.to_str()) != Some(EXTENSION)
            {
                continue;
            }
            let Some(network_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Some(exposures) = self.load(network_id).await? {
                entries.push((network_id.to_string(), exposures));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Remove one journal entry. A missing entry is success: delete is
    /// idempotent and must also clean up after a pre-journal upgrade.
    pub async fn remove(&self, network_id: &str) -> Result<(), ChvError> {
        crate::topology_store::validate_network_id(network_id)?;
        let target = self.path_for(network_id);
        match tokio::fs::remove_file(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(ChvError::Io {
                    path: target.to_string_lossy().to_string(),
                    source: e,
                })
            }
        }
        self.sync_dir().await
    }

    async fn sync_dir(&self) -> Result<(), ChvError> {
        match tokio::fs::File::open(&self.dir).await {
            Ok(dir) => dir.sync_all().await.map_err(|e| ChvError::Io {
                path: self.dir.to_string_lossy().to_string(),
                source: e,
            }),
            Err(e) => Err(ChvError::Io {
                path: self.dir.to_string_lossy().to_string(),
                source: e,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposures() -> StoredExposures {
        StoredExposures {
            exposures: vec![StoredExposure {
                safe_exposure_id: "exp-1".to_string(),
                protocol: "tcp".to_string(),
                external_port: 2222,
                target_ip: "10.200.0.50".to_string(),
                target_port: 22,
            }],
        }
    }

    #[tokio::test]
    async fn persist_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        store.persist("net-1", &exposures()).await.unwrap();

        assert_eq!(store.load("net-1").await.unwrap(), Some(exposures()));
        assert!(store.path_for("net-1").exists());
    }

    #[tokio::test]
    async fn persist_replaces_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        store.persist("net-1", &exposures()).await.unwrap();

        let mut updated = exposures();
        updated.exposures[0].external_port = 2223;
        store.persist("net-1", &updated).await.unwrap();

        assert_eq!(store.load("net-1").await.unwrap(), Some(updated));
    }

    #[tokio::test]
    async fn load_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        assert!(store.load("net-1").await.unwrap().is_none());
        assert!(store.load_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        store.persist("net-1", &exposures()).await.unwrap();
        store.remove("net-1").await.unwrap();
        assert!(!store.path_for("net-1").exists());
        store.remove("net-1").await.unwrap();
    }

    #[tokio::test]
    async fn persisting_an_empty_set_removes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        store.persist("net-1", &exposures()).await.unwrap();
        store
            .persist("net-1", &StoredExposures::default())
            .await
            .unwrap();
        assert!(!store.path_for("net-1").exists());
    }

    #[tokio::test]
    async fn load_all_is_sorted_and_ignores_temporary_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        store.persist("net-b", &exposures()).await.unwrap();
        store.persist("net-a", &exposures()).await.unwrap();
        tokio::fs::write(dir.path().join(".net-a.1.2.tmp"), b"junk")
            .await
            .unwrap();

        let all = store.load_all().await.unwrap();
        assert_eq!(
            all.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["net-a", "net-b"]
        );
    }

    #[tokio::test]
    async fn unsafe_network_id_is_rejected_before_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        assert!(matches!(
            store.persist("../escape", &exposures()).await,
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            store.load("../escape").await,
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            store.remove("../escape").await,
            Err(ChvError::InvalidArgument { .. })
        ));
    }

    #[tokio::test]
    async fn corrupt_entry_fails_loudly_instead_of_being_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        tokio::fs::write(store.path_for("net-1"), b"not json")
            .await
            .unwrap();

        let error = store.load("net-1").await.expect_err("corruption must fail");
        assert!(
            error.to_string().contains("corrupt exposure journal entry"),
            "unexpected error: {error}"
        );
        assert!(store.load_all().await.is_err());
    }

    #[tokio::test]
    async fn truncated_entry_with_missing_fields_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExposureStore::new(dir.path());
        // Parseable JSON that lost the protocol/port fields: replaying it
        // would install `dport 0` DNAT, so decode must fail instead.
        tokio::fs::write(
            store.path_for("net-1"),
            br#"{"exposures":[{"safe_exposure_id":"exp-1"}]}"#,
        )
        .await
        .unwrap();

        let error = store
            .load("net-1")
            .await
            .expect_err("a truncated exposure must fail to decode");
        assert!(
            error.to_string().contains("corrupt exposure journal entry"),
            "unexpected error: {error}"
        );

        // A journal object without the `exposures` key is corrupt too: an
        // empty set is represented by file absence, never by `{}`.
        tokio::fs::write(store.path_for("net-1"), b"{}")
            .await
            .unwrap();
        assert!(store.load("net-1").await.is_err());
    }
}
