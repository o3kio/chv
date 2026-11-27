use chv_config::AgentAuthorityMode;
use control_plane_node_api::control_plane_node_api as proto;
use std::path::Path;
use tracing::warn;

use crate::stord_backend::StordBackendInfo;

/// Node fabric identity reported by nwd's `GetFabricIdentity` RPC
/// (ADR-021). An empty public key / zero MTU means "no identity yet"
/// (nwd not up or fabric disabled); the agent re-reports on the next
/// inventory cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FabricIdentity {
    pub wireguard_public_key: String,
    pub underlay_mtu: u32,
}

/// Best-effort fetch of the node's fabric identity from the local nwd
/// daemon over its UDS socket.
///
/// Never fails: when nwd is unreachable or the RPC errors (e.g. during
/// enrollment, before nwd is up), a `warn!` is emitted and the empty
/// identity is returned so callers can keep building inventory.
pub async fn fetch_fabric_identity(nwd_socket: &Path) -> FabricIdentity {
    let mut nwd = match crate::daemon_clients::NwdClient::connect(nwd_socket).await {
        Ok(nwd) => nwd,
        Err(e) => {
            warn!(
                error = %e,
                "nwd unavailable for fabric identity; reporting empty fabric identity"
            );
            return FabricIdentity::default();
        }
    };
    match nwd.get_fabric_identity().await {
        Ok((wireguard_public_key, underlay_mtu)) => FabricIdentity {
            wireguard_public_key,
            underlay_mtu,
        },
        Err(e) => {
            warn!(
                error = %e,
                "failed to fetch fabric identity from nwd; reporting empty fabric identity"
            );
            FabricIdentity::default()
        }
    }
}

pub struct InventoryReporter {
    node_id: String,
    hostname: String,
    /// #379 DP4: the node's ACTUAL stord backend class, learned from the
    /// operator's `stord.toml` via `AgentConfig.stord_config_path` —
    /// never directory probing (the pre-#379 probe reported
    /// `["localdisk"]` on LVM nodes and listed a non-existent `nfs`;
    /// design §2.5). Defaults to `local` (the supervisor-managed stord's
    /// generated config never sets `backend_type`).
    stord_backend: StordBackendInfo,
    /// #378: the agent's authority mode (a static per-process config
    /// fact), reported on every inventory so the control plane can
    /// reject volume snapshot-family requests at accept time. Follows the
    /// config default (`core-managed`, fail-closed); callers that own the
    /// configured mode override it with [`Self::with_authority_mode`].
    authority_mode: AgentAuthorityMode,
}

impl InventoryReporter {
    pub fn new(node_id: impl Into<String>, hostname: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            hostname: hostname.into(),
            stord_backend: StordBackendInfo::default(),
            authority_mode: AgentAuthorityMode::default(),
        }
    }

    /// Set the reported stord backend info (#379 DP4) — the class the
    /// node's stord actually serves, from the same
    /// `stord_config_path`-parsed source the DP5 locator shaping uses.
    pub fn with_stord_backend(mut self, stord_backend: StordBackendInfo) -> Self {
        self.stord_backend = stord_backend;
        self
    }

    /// Set the reported authority mode from the agent's local config
    /// (#378).
    pub fn with_authority_mode(mut self, authority_mode: AgentAuthorityMode) -> Self {
        self.authority_mode = authority_mode;
        self
    }

    fn probe_kvm_available() -> bool {
        std::path::Path::new("/dev/kvm").exists()
    }

    pub fn build_inventory(&self) -> proto::NodeInventory {
        self.build_inventory_with_fabric_identity(FabricIdentity::default())
    }

    /// Build inventory carrying the node's fabric identity (ADR-021).
    /// Callers obtain it via [`fetch_fabric_identity`], which is
    /// best-effort and returns the empty identity when nwd is not up.
    pub fn build_inventory_with_fabric_identity(
        &self,
        fabric: FabricIdentity,
    ) -> proto::NodeInventory {
        let mut hypervisor_capabilities = Vec::new();
        if Self::probe_kvm_available() {
            hypervisor_capabilities.push("kvm".to_string());
        }

        proto::NodeInventory {
            node_id: self.node_id.clone(),
            hostname: self.hostname.clone(),
            architecture: std::env::consts::ARCH.to_string(),
            cpu_threads: std::thread::available_parallelism()
                .map(|n| n.get() as u64)
                .unwrap_or(0),
            memory_bytes: probe_memory_bytes(),
            // #379 DP4: the node's actual backend class (the stord
            // `backend_type` vocabulary — `local`/`iscsi`/`ceph`/`lvm`),
            // from the parsed stord config. The pre-#379 directory probe
            // is gone: it reported "localdisk" on LVM nodes and invented
            // an "nfs" class no stord backend serves.
            storage_classes: self.stord_backend.offered_storage_classes(),
            network_capabilities: vec![],
            labels: std::collections::HashMap::new(),
            hypervisor_capabilities,
            vtep_ip: "".to_string(),
            wireguard_public_key: fabric.wireguard_public_key,
            underlay_mtu: fabric.underlay_mtu,
            authority_mode: authority_mode_proto(self.authority_mode.clone()).into(),
        }
    }

    pub fn build_versions(&self) -> proto::ServiceVersions {
        proto::ServiceVersions {
            node_id: self.node_id.clone(),
            chv_agent_version: env!("CARGO_PKG_VERSION").to_string(),
            chv_stord_version: "".to_string(),
            chv_nwd_version: "".to_string(),
            cloud_hypervisor_version: "".to_string(),
            host_bundle_version: "".to_string(),
        }
    }
}

/// Map the agent config's authority mode onto the inventory proto enum
/// (#378). The control plane persists the reported mode on the node's
/// inventory row and uses it for accept-time policy checks.
fn authority_mode_proto(mode: AgentAuthorityMode) -> proto::AuthorityMode {
    match mode {
        AgentAuthorityMode::Legacy => proto::AuthorityMode::Legacy,
        AgentAuthorityMode::CoreManaged => proto::AuthorityMode::CoreManaged,
        AgentAuthorityMode::CoreNative => proto::AuthorityMode::CoreNative,
    }
}

fn parse_meminfo_total(content: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let rest = rest.trim();
            if let Some(kb_str) = rest.strip_suffix("kB") {
                if let Ok(kb) = kb_str.trim().parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

fn probe_memory_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .map(|c| parse_meminfo_total(&c))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn inventory_has_node_id() {
        let reporter = InventoryReporter::new("node-abc", "host-1");
        let inventory = reporter.build_inventory();
        assert_eq!(inventory.node_id, "node-abc");
        assert_eq!(inventory.hostname, "host-1");
    }

    #[test]
    fn inventory_reports_authority_mode_from_config() {
        // #378: the agent reports its authority mode (a static per-process
        // config fact) on every inventory so the CP can reject the volume
        // snapshot family at accept time. Default construction now follows
        // the fail-closed config default (core-managed); legacy is explicit.
        assert_eq!(
            InventoryReporter::new("n", "h")
                .build_inventory()
                .authority_mode,
            proto::AuthorityMode::CoreManaged as i32
        );
        for (mode, expected) in [
            (AgentAuthorityMode::Legacy, proto::AuthorityMode::Legacy),
            (
                AgentAuthorityMode::CoreManaged,
                proto::AuthorityMode::CoreManaged,
            ),
            (
                AgentAuthorityMode::CoreNative,
                proto::AuthorityMode::CoreNative,
            ),
        ] {
            let reporter = InventoryReporter::new("n", "h").with_authority_mode(mode.clone());
            assert_eq!(
                reporter.build_inventory().authority_mode,
                expected as i32,
                "config mode {mode:?} must be reported verbatim"
            );
        }
    }

    #[test]
    fn storage_classes_default_to_the_local_backend() {
        // #379 DP4: with no stord backend info, the node reports exactly
        // ["local"] — the supervisor-managed stord's backend. This is the
        // truthful replacement for the pre-#379 directory probe (which
        // reported whatever subdirectories existed under storage_base_dir,
        // inventing classes no stord backend serves).
        let reporter = InventoryReporter::new("n", "h");
        let inventory = reporter.build_inventory();
        assert_eq!(inventory.storage_classes, vec!["local"]);
    }

    #[test]
    fn storage_classes_report_the_configured_stord_backend() {
        // #379 DP4: an LVM node reports exactly ["lvm"] — the daemon's
        // single backend, never a probed directory list.
        let reporter = InventoryReporter::new("n", "h").with_stord_backend(StordBackendInfo {
            backend_class: "lvm".to_string(),
            lvm_volume_group: Some("vg-0".to_string()),
        });
        let inventory = reporter.build_inventory();
        assert_eq!(inventory.storage_classes, vec!["lvm"]);
    }

    #[test]
    fn cpu_threads_is_nonzero() {
        let reporter = InventoryReporter::new("node-x", "host-x");
        let inventory = reporter.build_inventory();
        assert!(
            inventory.cpu_threads > 0,
            "cpu_threads should be > 0 on any real machine"
        );
    }

    #[test]
    fn probe_memory_bytes_format() {
        let mock_meminfo = "MemTotal:       16384000 kB\nMemFree:         8000000 kB\n";
        let bytes = parse_meminfo_total(mock_meminfo);
        assert_eq!(bytes, 16384000 * 1024);
    }

    #[test]
    fn probe_memory_bytes_missing_entry_returns_zero() {
        let mock_meminfo = "MemFree:         8000000 kB\nSwapTotal:       2048000 kB\n";
        let bytes = parse_meminfo_total(mock_meminfo);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn inventory_defaults_to_empty_fabric_identity() {
        let reporter = InventoryReporter::new("node-abc", "host-1");
        let inventory = reporter.build_inventory();
        assert_eq!(inventory.wireguard_public_key, "");
        assert_eq!(inventory.underlay_mtu, 0);
        assert_eq!(inventory.vtep_ip, "");
    }

    #[test]
    fn inventory_carries_fabric_identity() {
        let reporter = InventoryReporter::new("node-abc", "host-1");
        let inventory = reporter.build_inventory_with_fabric_identity(FabricIdentity {
            wireguard_public_key: "pub-key-1".to_string(),
            underlay_mtu: 1500,
        });
        assert_eq!(inventory.wireguard_public_key, "pub-key-1");
        assert_eq!(inventory.underlay_mtu, 1500);
    }

    /// Best-effort contract: an unreachable nwd socket (e.g. during
    /// enrollment, before nwd is up) yields the empty identity instead of
    /// an error, so inventory construction never fails because of nwd.
    #[tokio::test]
    async fn fetch_fabric_identity_unreachable_nwd_returns_empty() {
        let dir = tempdir().unwrap();
        let identity = fetch_fabric_identity(&dir.path().join("no-such-nwd.sock")).await;
        assert_eq!(identity, FabricIdentity::default());
        assert_eq!(identity.wireguard_public_key, "");
        assert_eq!(identity.underlay_mtu, 0);
    }

    /// Best-effort contract: an nwd that answers the identity RPC with an
    /// in-band error (e.g. fabric disabled) also yields the empty identity.
    #[tokio::test]
    async fn fetch_fabric_identity_nwd_error_returns_empty() {
        use crate::daemon_clients::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        let service = MockFabricNwd {
            calls: std::sync::Arc::new(FabricNwdCalls::default()),
            public_key: String::new(),
            underlay_mtu: 0,
            identity_error: true,
        };
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                        service,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let identity = fetch_fabric_identity(&socket).await;
        assert_eq!(identity, FabricIdentity::default());
    }

    /// End-to-end identity reporting: a reachable nwd serving
    /// GetFabricIdentity populates the inventory fabric fields.
    #[tokio::test]
    async fn fetch_fabric_identity_populates_inventory_fields() {
        use crate::daemon_clients::fabric_test_support::{FabricNwdCalls, MockFabricNwd};

        let dir = tempdir().unwrap();
        let socket = dir.path().join("nwd.sock");

        let uds = tokio::net::UnixListener::bind(&socket).unwrap();
        let service = MockFabricNwd {
            calls: std::sync::Arc::new(FabricNwdCalls::default()),
            public_key: "wg-pub-key-1".to_string(),
            underlay_mtu: 1500,
            identity_error: false,
        };
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    chv_nwd_api::chv_nwd_api::network_service_server::NetworkServiceServer::new(
                        service,
                    ),
                )
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
                .await
                .ok();
        });

        let identity = fetch_fabric_identity(&socket).await;
        assert_eq!(identity.wireguard_public_key, "wg-pub-key-1");
        assert_eq!(identity.underlay_mtu, 1500);

        let reporter = InventoryReporter::new("node-abc", "host-1");
        let inventory = reporter.build_inventory_with_fabric_identity(identity);
        assert_eq!(inventory.wireguard_public_key, "wg-pub-key-1");
        assert_eq!(inventory.underlay_mtu, 1500);
    }
}
