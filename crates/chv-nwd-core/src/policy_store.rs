//! Durable journal for the last applied firewall/NAT policy (rc.8 blocker B9
//! follow-up).
//!
//! Recovering the topology table after an nwd restart is not enough for the
//! field plan's §6.4 contract ("nwd restart / network disruption → policy and
//! bridge re-materialize"). nftables state is kernel state: it survives a
//! process restart, but not a host reboot, and the in-memory
//! [`crate::handlers::NetworkServiceImpl`] policy record is what re-scopes the
//! interface guard when a NIC attaches. Without a durable copy, a restart
//! followed by a NIC attach would leave the new tap outside the CHV
//! default-deny boundary (the base hooks are `policy accept` by design; only
//! the guard set dispatches guest traffic into the policy chains).
//!
//! This journal stores the last requested firewall/NAT policy JSON and version
//! per network under `/var/lib/chv/nwd/policies/<network_id>.json`.
//! `NetworkServiceImpl::recover_topologies` loads it after re-ensuring the
//! topology and re-applies it fail-closed, so a reboot or restart restores the
//! same guard set instead of leaving guests unfiltered.
//!
//! Persistence is desired-state: the handler records a policy even when the
//! executor's apply failed, matching the existing `refresh_policy_scope`
//! convergence rule (#227 S3). A corrupt entry fails daemon startup loudly for
//! the same reason a corrupt topology entry does.

use chv_errors::ChvError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Persistent policy journal root. Matches the `ExecStartPre=` directory in
/// `packaging/systemd/chv-nwd.service`.
pub const DEFAULT_POLICY_STATE_DIR: &str = "/var/lib/chv/nwd/policies";

const EXTENSION: &str = "json";

/// Monotonic suffix for the temporary sibling of an atomic write (same
/// collision-free scheme as the topology journal).
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The last requested policy pair for one network. JSON payloads are kept as
/// raw bytes exactly as nwd received them, so replay is byte-identical.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredPolicies {
    pub firewall: Option<Vec<u8>>,
    pub firewall_version: Option<String>,
    pub nat: Option<Vec<u8>>,
    pub nat_version: Option<String>,
}

impl StoredPolicies {
    pub fn is_empty(&self) -> bool {
        self.firewall.is_none() && self.nat.is_none()
    }
}

/// Filesystem-backed journal of requested firewall/NAT policy.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    dir: PathBuf,
}

impl PolicyStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Journal root used by the production daemon.
    pub fn default_dir() -> Self {
        Self::new(DEFAULT_POLICY_STATE_DIR)
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

    /// Atomically persist one network's requested policy pair.
    pub async fn persist(
        &self,
        network_id: &str,
        policies: &StoredPolicies,
    ) -> Result<(), ChvError> {
        crate::topology_store::validate_network_id(network_id)?;
        if policies.is_empty() {
            return self.remove(network_id).await;
        }
        self.ensure_dir().await?;

        let bytes = serde_json::to_vec(policies).map_err(|e| ChvError::Internal {
            reason: format!("failed to encode policy journal entry for {network_id}: {e}"),
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
    pub async fn load(&self, network_id: &str) -> Result<Option<StoredPolicies>, ChvError> {
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
        let policies = serde_json::from_slice(&bytes).map_err(|e| ChvError::Internal {
            reason: format!("corrupt policy journal entry {}: {e}", path.display()),
        })?;
        Ok(Some(policies))
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

    fn policies() -> StoredPolicies {
        StoredPolicies {
            firewall: Some(br#"{"rules":[]}"#.to_vec()),
            firewall_version: Some("policy-7".to_string()),
            nat: Some(b"[]".to_vec()),
            nat_version: Some("policy-7".to_string()),
        }
    }

    #[tokio::test]
    async fn persist_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        store.persist("net-1", &policies()).await.unwrap();

        assert_eq!(store.load("net-1").await.unwrap(), Some(policies()));
        assert!(store.path_for("net-1").exists());
    }

    #[tokio::test]
    async fn persist_replaces_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        store.persist("net-1", &policies()).await.unwrap();

        let mut updated = policies();
        updated.firewall = Some(br#"{"rules":[{"action":"drop"}]}"#.to_vec());
        store.persist("net-1", &updated).await.unwrap();

        assert_eq!(store.load("net-1").await.unwrap(), Some(updated));
    }

    #[tokio::test]
    async fn load_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        assert!(store.load("net-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        store.persist("net-1", &policies()).await.unwrap();
        store.remove("net-1").await.unwrap();
        assert!(!store.path_for("net-1").exists());
        store.remove("net-1").await.unwrap();
    }

    #[tokio::test]
    async fn persisting_an_empty_pair_removes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        store.persist("net-1", &policies()).await.unwrap();
        store
            .persist("net-1", &StoredPolicies::default())
            .await
            .unwrap();
        assert!(!store.path_for("net-1").exists());
    }

    #[tokio::test]
    async fn unsafe_network_id_is_rejected_before_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::new(dir.path());
        assert!(matches!(
            store.persist("../escape", &policies()).await,
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
        let store = PolicyStore::new(dir.path());
        tokio::fs::write(store.path_for("net-1"), b"not json")
            .await
            .unwrap();

        let error = store.load("net-1").await.expect_err("corruption must fail");
        assert!(
            error.to_string().contains("corrupt policy journal entry"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn default_dir_matches_the_unit_contract() {
        assert_eq!(DEFAULT_POLICY_STATE_DIR, "/var/lib/chv/nwd/policies");
        assert_eq!(
            PolicyStore::default_dir().dir(),
            Path::new(DEFAULT_POLICY_STATE_DIR)
        );
    }
}
