//! Durable journal for bridge-only network topologies (rc.8 blocker B9).
//!
//! `chv-nwd` used to treat its in-memory [`crate::state::TopologyTable`] as
//! the only ownership record. After any documented restart the table was
//! empty, so the next `SetFirewallPolicy` failed closed with
//! `not found: topology <id>` even though the bridge, TAPs and nft table were
//! still on the host. That made "nwd restart / network disruption → policy and
//! bridge re-materialize" (field plan §6.4) unprovable and required hidden
//! maintainer knowledge to recover (§6.9).
//!
//! This module persists the exact `TopologySpec` protobuf that produced a
//! successful apply, atomically, under
//! `/var/lib/chv/nwd/topologies/<network_id>.pb`. `NetworkServer::serve`
//! replays every entry before it starts serving. Only bridge-only topologies
//! are journaled: the stretched-L2 fabric is design-only in this release and
//! already has its own durable ownership journal, so persisting a fabric plan
//! here would claim a recovery surface that does not exist.
//!
//! This is deliberately *not* the removed SQLite `TopologyStore`: there is no
//! second authority, no mutable state machine and no "best effort" write path.
//! A failed persist fails the ensure RPC (the caller retries the idempotent
//! apply), and a corrupt entry fails daemon startup loudly instead of silently
//! serving a partial table.

use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api as proto;
use prost::Message;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Persistent topology journal root. Matches the `ExecStartPre=` directory in
/// `packaging/systemd/chv-nwd.service`; the unit creates it 0700 owned by
/// `chv`, and the daemon can also create it when started by the agent
/// supervisor without systemd.
pub const DEFAULT_TOPOLOGY_STATE_DIR: &str = "/var/lib/chv/nwd/topologies";

const EXTENSION: &str = "pb";

/// Monotonic suffix for the temporary sibling of an atomic write. The
/// process-wide value plus the pid makes collisions impossible without a
/// lock; concurrent writers for the same network still end with one complete
/// file because `rename(2)` is atomic.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Validate an id before it is used as a file name. The same allowlist the
/// executor and DHCP paths apply, kept here so a journal write can never
/// traverse outside the state directory.
pub(crate) fn validate_network_id(network_id: &str) -> Result<(), ChvError> {
    if network_id.is_empty()
        || !network_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(ChvError::InvalidArgument {
            field: "network_id".to_string(),
            reason: format!("network_id contains invalid characters: {network_id}"),
        });
    }
    Ok(())
}

/// Filesystem-backed journal of applied bridge-only topology specs.
#[derive(Debug, Clone)]
pub struct TopologyStore {
    dir: PathBuf,
}

impl TopologyStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Journal root used by the production daemon.
    pub fn default_dir() -> Self {
        Self::new(DEFAULT_TOPOLOGY_STATE_DIR)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, network_id: &str) -> PathBuf {
        self.dir.join(format!("{network_id}.{EXTENSION}"))
    }

    /// Create the journal directory if needed. Production systemd creates it
    /// via `ExecStartPre=`; the supervisor-spawned daemon may not have it.
    async fn ensure_dir(&self) -> Result<(), ChvError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| ChvError::Io {
                path: self.dir.to_string_lossy().to_string(),
                source: e,
            })?;
        // Keep the journal private even when the directory was created here
        // (the unit's `install -d -m 0700` already does this in production).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).await;
        }
        Ok(())
    }

    /// Atomically persist one bridge-only topology spec. The caller must
    /// treat a failure as a failed ensure: without the journal, a restart
    /// would lose the topology the RPC just claimed to own.
    pub async fn persist(&self, spec: &proto::TopologySpec) -> Result<(), ChvError> {
        validate_network_id(&spec.network_id)?;
        self.ensure_dir().await?;

        let mut bytes = Vec::with_capacity(spec.encoded_len());
        spec.encode(&mut bytes).map_err(|e| ChvError::Internal {
            reason: format!(
                "failed to encode topology spec for {}: {e}",
                spec.network_id
            ),
        })?;

        let target = self.path_for(&spec.network_id);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            spec.network_id,
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
            // The journal is a recovery contract: flush the payload before
            // the rename publishes it, so a crash cannot leave the name bound
            // to an empty file.
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
            // Rename is only atomic in the running kernel's view until the
            // directory entry itself is durable. The journal is a recovery
            // contract across a host reboot, so flush the directory too.
            self.sync_dir().await
        }
        .await;

        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        write_result
    }

    /// Remove one journal entry. A missing entry is success: delete is
    /// idempotent and must also clean up after a pre-journal upgrade.
    pub async fn remove(&self, network_id: &str) -> Result<(), ChvError> {
        validate_network_id(network_id)?;
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
        // Mirror `persist`: an unlink that is not durable could reappear
        // after a host crash, and startup would then resurrect a network the
        // operator deleted. The caller treats a failed remove as a failed
        // delete, so the idempotent retry can clear it.
        self.sync_dir().await
    }

    /// Read one journal entry, if present. Used by the idempotent ensure path
    /// to avoid rewriting an unchanged journal entry on every reconciliation
    /// tick while still repairing a missing or divergent one.
    pub async fn load(&self, network_id: &str) -> Result<Option<proto::TopologySpec>, ChvError> {
        validate_network_id(network_id)?;
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
        let spec =
            proto::TopologySpec::decode(bytes.as_slice()).map_err(|e| ChvError::Internal {
                reason: format!("corrupt topology journal entry {}: {e}", path.display()),
            })?;
        Ok(Some(spec))
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

    /// Load every journaled spec in deterministic (file name) order.
    ///
    /// A decode or I/O failure is returned, not skipped: silently dropping an
    /// entry would resurrect the exact "topology not found after restart" bug
    /// this journal exists to prevent. Startup fails loudly and the operator
    /// can inspect the named file.
    pub async fn load_all(&self) -> Result<Vec<proto::TopologySpec>, ChvError> {
        self.ensure_dir().await?;
        let mut read_dir = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|e| ChvError::Io {
                path: self.dir.to_string_lossy().to_string(),
                source: e,
            })?;

        let mut entries = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| ChvError::Io {
            path: self.dir.to_string_lossy().to_string(),
            source: e,
        })? {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            // Temporary siblings from an interrupted atomic write are not
            // journal entries; a stale one is harmless and gets replaced.
            if !name.ends_with(&format!(".{EXTENSION}")) {
                continue;
            }
            entries.push((name.to_string(), entry.path()));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut specs = Vec::with_capacity(entries.len());
        for (name, path) in entries {
            let bytes = tokio::fs::read(&path).await.map_err(|e| ChvError::Io {
                path: path.to_string_lossy().to_string(),
                source: e,
            })?;
            let spec =
                proto::TopologySpec::decode(bytes.as_slice()).map_err(|e| ChvError::Internal {
                    reason: format!("corrupt topology journal entry {}: {e}", path.display()),
                })?;
            let expected = format!("{}.{EXTENSION}", spec.network_id);
            if name != expected {
                return Err(ChvError::Internal {
                    reason: format!(
                        "topology journal entry {} names network {} (expected {}); \
                         refusing to recover an ambiguous topology",
                        path.display(),
                        spec.network_id,
                        expected
                    ),
                });
            }
            specs.push(spec);
        }
        Ok(specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(network_id: &str) -> proto::TopologySpec {
        proto::TopologySpec {
            network_id: network_id.to_string(),
            tenant_id: String::new(),
            bridge_name: chv_common::bridge_name_for_network(network_id),
            namespace_name: format!("ns-{network_id}"),
            subnet_cidr: "10.200.0.0/24".to_string(),
            gateway_ip: "10.200.0.1".to_string(),
            options: Default::default(),
            vni: 0,
            vtep_endpoints: vec![],
            overlay_type: 0,
            fabric: None,
        }
    }

    #[tokio::test]
    async fn persist_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        let original = spec("net-1");

        store.persist(&original).await.unwrap();
        assert!(store.path_for("net-1").exists());

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded, vec![original]);
    }

    #[tokio::test]
    async fn persist_is_idempotent_and_replaces_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        store.persist(&spec("net-1")).await.unwrap();

        let mut updated = spec("net-1");
        updated.subnet_cidr = "10.201.0.0/24".to_string();
        updated.gateway_ip = "10.201.0.1".to_string();
        store.persist(&updated).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].subnet_cidr, "10.201.0.0/24");
    }

    #[tokio::test]
    async fn load_reads_one_entry_and_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        assert!(store.load("net-1").await.unwrap().is_none());

        store.persist(&spec("net-1")).await.unwrap();
        assert_eq!(store.load("net-1").await.unwrap(), Some(spec("net-1")));
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        store.persist(&spec("net-1")).await.unwrap();

        store.remove("net-1").await.unwrap();
        assert!(!store.path_for("net-1").exists());
        // Deleting an entry that was never journaled (pre-journal upgrade)
        // must not turn a topology delete into an error.
        store.remove("net-1").await.unwrap();
    }

    #[tokio::test]
    async fn unsafe_network_id_is_rejected_before_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        let mut bad = spec("../escape");
        bad.network_id = "../escape".to_string();

        assert!(matches!(
            store.persist(&bad).await,
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            store.remove("../escape").await,
            Err(ChvError::InvalidArgument { .. })
        ));
    }

    #[tokio::test]
    async fn load_all_ignores_temporary_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        store.persist(&spec("net-1")).await.unwrap();
        tokio::fs::write(dir.path().join(".net-1.1.2.tmp"), b"partial")
            .await
            .unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].network_id, "net-1");
    }

    #[tokio::test]
    async fn corrupt_entry_fails_loudly_instead_of_being_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        tokio::fs::write(store.path_for("net-1"), b"not a protobuf")
            .await
            .unwrap();

        let error = store.load_all().await.expect_err("corruption must fail");
        assert!(
            error.to_string().contains("corrupt topology journal entry"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn entry_whose_name_does_not_match_its_network_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = TopologyStore::new(dir.path());
        // Encode correctly but at the wrong path: recovering it would bind
        // one network's spec to another network's identity.
        let mut bytes = Vec::new();
        spec("net-2").encode(&mut bytes).unwrap();
        tokio::fs::write(store.path_for("net-1"), bytes)
            .await
            .unwrap();

        let error = store
            .load_all()
            .await
            .expect_err("ambiguous entry must fail");
        assert!(
            error.to_string().contains("refusing to recover"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn default_dir_matches_the_unit_contract() {
        assert_eq!(DEFAULT_TOPOLOGY_STATE_DIR, "/var/lib/chv/nwd/topologies");
        assert_eq!(
            TopologyStore::default_dir().dir(),
            Path::new(DEFAULT_TOPOLOGY_STATE_DIR)
        );
    }
}
