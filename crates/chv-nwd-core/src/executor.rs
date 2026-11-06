use async_trait::async_trait;
use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api::{FabricPlan, OverlayType, TopologySpec};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::fabric::{AppliedFabric, FabricHandle, FabricIdentity};

// Metric names for network daemon operations.
const NWD_NFT_ERRORS_TOTAL: &str = "chv_nwd_nft_errors_total";
const NWD_DHCP_ERRORS_TOTAL: &str = "chv_nwd_dhcp_errors_total";

/// MTU a bridge-only topology runs at: a fresh bridge-only ensure never
/// sets an MTU, so the kernel default for a new bridge (1500) is the
/// authoritative value. Used to reset a previously fabric-backed bridge
/// when it is re-ensured bridge-only (m7 residue).
const BRIDGE_ONLY_DEFAULT_MTU: u32 = 1500;

/// Poll interval of the bounded dnsmasq exit wait (m8 race).
const DNSMASQ_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Budget for the bounded dnsmasq exit wait after SIGTERM (m8 race):
/// dnsmasq runs with `bind-interfaces`, so a replacement started while
/// the old process still holds the listen address fails with
/// NetworkUnavailable ("address already in use") — after the fabric
/// re-apply already succeeded.
const DNSMASQ_EXIT_TERM_BUDGET: Duration = Duration::from_secs(2);
/// Brief final wait after SIGKILL, so a fast re-ensure still finds the
/// listen address free in the common case.
const DNSMASQ_EXIT_KILL_BUDGET: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct TopologyApplyResult {
    pub namespace_handle: String,
    pub bridge_handle: String,
    /// Tenant MTU applied by the fabric path (advertised via DHCP option 26
    /// and set on the bridge/TAPs). `None` for bridge-only topologies.
    pub tenant_mtu: Option<u32>,
    /// Fabric plan generation applied by the fabric path, for stale-plan
    /// fencing on later updates. `None` for bridge-only topologies.
    pub fabric_plan_generation: Option<u64>,
    /// VNI binding generation applied by the fabric path (ADR-021 §8), for
    /// stale-binding fencing on later updates. `None` for bridge-only
    /// topologies.
    pub binding_generation: Option<u64>,
}

/// Overlay status as observed by the stretched-L2 fabric path (ADR-021).
/// `fdb_entry_count` reports the network's head-end-replication flood
/// peers; the legacy per-VTEP FDB datapath was retired by ADR-021.
#[derive(Debug, Clone)]
pub struct OverlayStatusInfo {
    pub vxlan_interface_up: bool,
    pub fdb_entry_count: u32,
}

/// Outcome of a topology delete (n11): the overall `Result` of
/// `NetworkExecutor::delete_topology` reports the LOCAL teardown, while
/// the fabric-half teardown is fail-open inside the executor and its
/// outcome is reported separately — so the handler can count
/// `nwd_fabric_remove_total` truthfully instead of proxying it from the
/// aggregate result (the old approximation counted a fabric success even
/// when the fail-open fabric removal had failed, and counted a fabric
/// failure on a purely local teardown error).
///
/// `fabric_removed` is:
/// - `None` — no fabric teardown was attempted: the topology had no
///   applied fabric plan, or the fabric provider is disabled in nwd
///   configuration (the executor already warns; there is no removal
///   whose outcome could be counted);
/// - `Some(Ok(()))` — the fabric overlay was removed;
/// - `Some(Err(e))` — removal was attempted and failed; local teardown
///   continued (fail-open for teardown only — apply stays fail-closed)
///   and the residue stays visible via the provider ownership journal.
#[derive(Debug)]
pub struct DeleteOutcome {
    pub fabric_removed: Option<Result<(), ChvError>>,
}

/// Result of a fabric ownership probe (used by the no-state-row delete
/// path, M3): distinguishes "the enabled provider holds no entry" from
/// "the provider is disabled in configuration", so the residue case
/// (fabric applied, nwd restarted with the fabric provider disabled,
/// delete) is loudly visible instead of a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricOwnership {
    /// The provider's durable ownership journal holds the network.
    Owned,
    /// The provider is enabled and holds no entry for the network.
    NotOwned,
    /// The fabric provider is disabled in nwd configuration: ownership
    /// cannot be observed in this process.
    ProviderDisabled,
}

#[async_trait]
pub trait NetworkExecutor: Send + Sync + 'static {
    async fn ensure_topology(&self, spec: &TopologySpec) -> Result<TopologyApplyResult, ChvError>;

    async fn delete_topology(
        &self,
        network_id: &str,
        state: &crate::state::TopologyState,
    ) -> Result<DeleteOutcome, ChvError>;

    /// Best-effort LOCAL topology teardown by derived names only (#356 N5).
    ///
    /// For deletes that arrive with no in-memory `TopologyTable` row (an
    /// nwd restart wiped it): the namespace, bridge, dnsmasq files, and
    /// nft table are all derived deterministically from `network_id`, so
    /// the local half of the teardown can still run — mirroring the fabric
    /// half's ownership-journal fallback. Existence checks make this a
    /// no-op for a network that never materialized on this node; every
    /// failure is warned and skipped (fail-open for teardown only, exactly
    /// like the state-row path).
    async fn delete_local_topology_by_derived_names(&self, network_id: &str);

    async fn health(
        &self,
        network_id: &str,
        state: &crate::state::TopologyState,
    ) -> Result<String, ChvError>;

    #[allow(clippy::too_many_arguments)]
    async fn attach_vm_nic(
        &self,
        network_id: &str,
        nic_id: &str,
        vm_id: &str,
        bridge_name: &str,
        tenant_mtu: Option<u32>,
        mac_address: &str,
        ip_address: &str,
    ) -> Result<(String, String), ChvError>;

    async fn detach_vm_nic(
        &self,
        nic_id: &str,
        ownership: chv_common::AttachmentOwnership,
    ) -> Result<(), ChvError>;

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError>;

    async fn set_nat_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError>;

    async fn ensure_dhcp_scope(
        &self,
        network_id: &str,
        cidr: &str,
        range_start: &str,
        range_end: &str,
        dns_servers: &[String],
    ) -> Result<(), ChvError>;

    async fn ensure_dns_scope(
        &self,
        network_id: &str,
        forwarders: &[&str],
        static_records: &std::collections::HashMap<String, String>,
    ) -> Result<(), ChvError>;

    #[allow(clippy::too_many_arguments)]
    async fn expose_service(
        &self,
        network_id: &str,
        exposure_id: &str,
        protocol: &str,
        external_port: u32,
        target_ip: &str,
        target_port: u32,
        mode: &str,
    ) -> Result<(), ChvError>;

    async fn withdraw_service_exposure(
        &self,
        network_id: &str,
        exposure_id: &str,
    ) -> Result<(), ChvError>;

    // --- Gratuitous ARP (ADR-021: flushes stale MAC/ARP caches after a
    // migration; the fabric's kernel MAC learning repopulates them) ---

    async fn send_gratuitous_arp(
        &self,
        namespace: &str,
        bridge_name: &str,
        vm_ip: &str,
    ) -> Result<(), ChvError>;

    // --- Stretched-L2 fabric methods (ADR-021) ---

    /// Realize a fabric plan for a network and graft the fabric consumer
    /// veth into the tenant bridge (enslave + MTU). The caller is
    /// responsible for generation fencing before invoking this.
    async fn apply_fabric_overlay(
        &self,
        network_id: &str,
        vni: u32,
        plan: &FabricPlan,
        bridge_name: &str,
    ) -> Result<AppliedFabric, ChvError>;

    /// Tear down one network's fabric state (reverse dependency order,
    /// preserves the WireGuard key). Called before local topology teardown.
    async fn remove_fabric_overlay(&self, network_id: &str) -> Result<(), ChvError>;

    /// Whether the fabric provider's durable ownership journal holds an
    /// entry for this network — used by the delete path to clean fabric
    /// residue after an nwd restart wiped the topology table. Returns
    /// [`FabricOwnership::ProviderDisabled`] when the provider is
    /// disabled in configuration, so the caller can warn about the
    /// unobservable residue instead of silently no-op'ing.
    async fn fabric_owned(&self, network_id: &str) -> Result<FabricOwnership, ChvError>;

    /// The node's public fabric identity (WireGuard public key + measured
    /// underlay MTU). Fails closed when the fabric provider is disabled.
    async fn fabric_identity(&self) -> Result<FabricIdentity, ChvError>;

    /// Observed fabric overlay status for a network.
    async fn fabric_overlay_status(&self, network_id: &str) -> Result<OverlayStatusInfo, ChvError>;

    /// Re-assert the tenant MTU of a running topology on the bridge and
    /// every currently enslaved port, then restart the network's dnsmasq
    /// so DHCP option 26 advertises the new value (m8). Only invoked on
    /// an MTU change.
    ///
    /// `None` resets a previously fabric-backed topology to the
    /// bridge-only defaults (m7 residue): the bridge and ports are set
    /// to [`BRIDGE_ONLY_DEFAULT_MTU`] and dnsmasq is restarted WITHOUT
    /// DHCP option 26, matching what a fresh bridge-only ensure leaves
    /// behind.
    #[allow(clippy::too_many_arguments)]
    async fn reassert_tenant_mtu(
        &self,
        network_id: &str,
        bridge_name: &str,
        subnet_cidr: &str,
        gateway_ip: &str,
        tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError>;
}

/// A service exposure tracked by the executor so the DNAT forward-accept rule
/// can be re-asserted after every firewall apply (which rebuilds the `forward`
/// base chain and would otherwise silently drop exposed flows into default-deny).
#[derive(Clone)]
struct ExposureSpec {
    safe_exposure_id: String,
    protocol: String,
    external_port: u32,
    target_ip: String,
    target_port: u32,
}

pub struct LinuxExecutor {
    _runtime_dir: PathBuf,
    /// Stretched-L2 fabric provider (ADR-021). `None` = fabric disabled;
    /// every fabric RPC fails closed in that state.
    fabric: Option<Arc<dyn FabricHandle>>,
    /// Serializes all nft table mutations for this executor (firewall/NAT
    /// apply, service exposure, topology create/delete). The filter and NAT
    /// paths flush+rebuild chains on the per-network table, so concurrent
    /// writers could interleave and leave the table without its terminal
    /// default-deny rules (fail-open for CHV guests). A single writer per
    /// executor closes that race (#227).
    ///
    /// Note: `ensure_topology`/`delete_topology` hold the lock across slow
    /// topology work (dnsmasq spawn, VXLAN/FDB teardown), so a firewall/NAT
    /// apply can briefly queue behind topology operations. This is acceptable
    /// for a control-plane daemon and avoids a finer-grained per-network lock.
    nft_lock: Arc<Mutex<()>>,
    /// Per-network service exposures (keyed by network_id) for re-assertion
    /// after firewall applies.
    ///
    /// NOTE (documented limitation): this is in-memory only — a daemon restart
    /// loses it. On restart, an operator must re-declare exposures (the DNAT
    /// prerouting rule also does not survive a restart unless re-applied by the
    /// caller). Persisting exposures across restarts is tracked separately from
    /// #227 and out of scope for this change.
    exposures: Arc<DashMap<String, Vec<ExposureSpec>>>,
}

/// Owned-argument helper for command sequences held as `Vec<String>`.
fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_string()).collect()
}

impl LinuxExecutor {
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            _runtime_dir: runtime_dir,
            fabric: None,
            nft_lock: Arc::new(Mutex::new(())),
            exposures: Arc::new(DashMap::new()),
        }
    }

    /// Attach a fabric provider, enabling the stretched-L2 fabric paths
    /// (ADR-021). Without this, fabric requests fail closed.
    pub fn with_fabric(mut self, fabric: Arc<dyn FabricHandle>) -> Self {
        self.fabric = Some(fabric);
        self
    }

    fn fabric_handle(&self) -> Result<&Arc<dyn FabricHandle>, ChvError> {
        self.fabric.as_ref().ok_or_else(|| ChvError::InvalidArgument {
            field: "fabric".to_string(),
            reason: "fabric overlay requested but the fabric provider is disabled in nwd configuration"
                .to_string(),
        })
    }

    /// Host-namespace command sequence grafting the fabric consumer veth
    /// into the tenant bridge after a successful fabric apply (ADR-021):
    /// enslave, set the tenant MTU on bridge and veth, then bring the veth
    /// up (the host side may still be down after a provider re-apply).
    fn fabric_attach_commands(
        consumer_veth: &str,
        bridge_name: &str,
        tenant_mtu: u32,
    ) -> Vec<Vec<String>> {
        let mtu = tenant_mtu.to_string();
        vec![
            string_args(&["link", "set", consumer_veth, "master", bridge_name]),
            string_args(&["link", "set", bridge_name, "mtu", mtu.as_str()]),
            string_args(&["link", "set", consumer_veth, "mtu", mtu.as_str()]),
            string_args(&["link", "set", consumer_veth, "up"]),
        ]
    }

    /// Host-namespace command sequence re-asserting the tenant MTU on the
    /// bridge and every currently enslaved port (m8). `owned` is the
    /// bridge plus its members as resolved by
    /// [`LinuxExecutor::owned_ifaces_for_bridge`]; the fabric consumer
    /// veth is a bridge member, so it is covered by the enumeration (the
    /// provider also re-asserts it on apply).
    fn port_mtu_commands(owned_ifaces: &[String], tenant_mtu: u32) -> Vec<Vec<String>> {
        let mtu = tenant_mtu.to_string();
        owned_ifaces
            .iter()
            .map(|dev| string_args(&["link", "set", "dev", dev, "mtu", mtu.as_str()]))
            .collect()
    }

    async fn run_ip(args: &[&str]) -> Result<(), ChvError> {
        let out = Command::new("ip")
            .args(args)
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "ip".to_string(),
                source: e,
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("File exists") || stderr.contains("already exists") {
                return Ok(());
            }
            return Err(ChvError::NetworkUnavailable {
                resource: "ip".to_string(),
                reason: format!("ip {} failed: {}", args.join(" "), stderr),
            });
        }
        Ok(())
    }

    async fn run_cmd_netns_output(
        namespace: &str,
        cmd: &str,
        args: &[&str],
    ) -> Result<std::process::Output, ChvError> {
        Command::new("ip")
            .args(["netns", "exec", namespace, cmd])
            .args(args)
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: cmd.to_string(),
                source: e,
            })
    }

    async fn bridge_exists(name: &str) -> bool {
        Command::new("ip")
            .args(["link", "show", "dev", name])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    async fn namespace_exists(name: &str) -> bool {
        std::path::Path::new("/var/run/netns").join(name).exists()
    }

    /// Extract the device name from one `ip link show master <bridge>` output line.
    ///
    /// Real-world line shapes (verified on iproute2 6.x):
    ///   355: vA@vB: <BROADCAST,MULTICAST,M-DOWN> ... master brsmp ...
    ///   388: tap-12ab: <BROADCAST,...> ... master brsmp ...
    ///
    /// Field 0 is the ifindex (`355:`), field 1 is `NAME@PEER:` — the actual
    /// interface name with an optional `@peer` suffix and a trailing colon.
    /// Parsing field 0 (the ifindex) instead would silently exclude every real
    /// member from the CHV-owned guard set (fail-open under br_netfilter).
    fn parse_ip_link_master_line(line: &str) -> Option<String> {
        // Only a `N:` ifindex header line introduces a device. Continuation
        // lines inside `ip link show master <br>` output (`link/ether ...`,
        // `altname ...`, `inet ...`) MUST be ignored: a token pulled from them
        // (e.g. `altname enp0s18` → `enp0s18`) could otherwise widen the
        // CHV-owned guard set to a real host interface on a later apply.
        let header = line.split_whitespace().next()?;
        let index_field = header.strip_suffix(':')?;
        if index_field.is_empty() || !index_field.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let name = line.split_whitespace().nth(1)?;
        let name = name.split(':').next()?; // drop trailing ':'
        let name = name.split('@').next()?; // drop veth @peer suffix
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    }

    /// Resolve the authoritative CHV-owned interface set for a topology bridge:
    /// the bridge itself plus any enslaved member devices (VM TAPs/veths).
    ///
    /// Fails closed when the topology-owned bridge does not exist, so CHV never
    /// guesses a host interface to scope firewall/NAT policy against (#227).
    async fn owned_ifaces_for_bridge(bridge_name: &str) -> Result<Vec<String>, ChvError> {
        if !Self::bridge_exists(bridge_name).await {
            return Err(ChvError::NotFound {
                resource: "bridge".to_string(),
                id: bridge_name.to_string(),
            });
        }
        let out = Command::new("ip")
            .args(["link", "show", "master", bridge_name])
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "ip".to_string(),
                source: e,
            })?;
        if !out.status.success() {
            return Err(ChvError::NetworkUnavailable {
                resource: "ip".to_string(),
                reason: format!("ip link show master {} failed", bridge_name),
            });
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut owned: Vec<String> = vec![bridge_name.to_string()];
        for line in stdout.lines() {
            if let Some(dev) = Self::parse_ip_link_master_line(line) {
                if !dev.is_empty() && dev != bridge_name {
                    owned.push(dev);
                }
            }
        }
        owned.sort();
        owned.dedup();
        Ok(owned)
    }

    fn tap_name_for_nic(nic_id: &str) -> String {
        // Linux interface names are limited to 15 bytes (IFNAMSIZ - 1).
        // Derive a stable compact tap name from the nic_id so very long IDs
        // (e.g. UUID-derived values) do not break `ip tuntap add`.
        let hash = chv_common::fnv1a_hash(nic_id);
        format!("tap-{:08x}", (hash & 0xffff_ffff) as u32)
    }

    async fn run_nft(args: &[&str]) -> Result<(), ChvError> {
        let out = Command::new("nft")
            .args(args)
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "nft".to_string(),
                source: e,
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ChvError::NetworkUnavailable {
                resource: "nft".to_string(),
                reason: format!("nft {} failed: {}", args.join(" "), stderr),
            });
        }
        Ok(())
    }

    async fn delete_rules_by_comment(
        table: &str,
        chain: &str,
        comment: &str,
    ) -> Result<(), ChvError> {
        let out = Command::new("nft")
            .args(["-a", "list", "chain", "inet", table, chain])
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "nft".to_string(),
                source: e,
            })?;
        if !out.status.success() {
            return Ok(()); // chain may not exist
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let target = format!("comment \"{}\"", comment);
        for line in stdout.lines() {
            if line.contains(&target) {
                if let Some(idx) = line.rfind(" handle ") {
                    let handle = line[idx + 8..].split_whitespace().next().unwrap_or("");
                    if !handle.is_empty() {
                        Self::run_nft(&["delete", "rule", "inet", table, chain, "handle", handle])
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    fn sanitize_id(id: &str) -> Result<String, ChvError> {
        if id.is_empty() {
            return Err(ChvError::InvalidArgument {
                field: "id".to_string(),
                reason: "id must not be empty".to_string(),
            });
        }
        if id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            Ok(id.to_string())
        } else {
            Err(ChvError::InvalidArgument {
                field: "id".to_string(),
                reason: format!("id contains invalid characters: {}", id),
            })
        }
    }

    fn sanitized_nft_table(network_id: &str) -> Result<String, ChvError> {
        let sanitized = Self::sanitize_id(network_id)?;
        Ok(format!("chv-{}", sanitized))
    }

    async fn run_nft_quiet(args: &[&str]) -> Result<(), ()> {
        match Command::new("nft").args(args).output().await {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!(args = ?args, stderr = %stderr, "nft command failed (non-fatal)");
                Ok(())
            }
            Err(e) => {
                tracing::debug!(args = ?args, error = %e, "nft command execution failed (non-fatal)");
                Ok(())
            }
            Ok(_) => Ok(()),
        }
    }

    async fn run_nft_idempotent(args: &[&str]) -> Result<(), ChvError> {
        match Self::run_nft(args).await {
            Ok(()) => Ok(()),
            Err(ChvError::NetworkUnavailable { reason, .. }) => {
                if reason.contains("File exists") || reason.contains("already exists") {
                    Ok(())
                } else {
                    Err(ChvError::NetworkUnavailable {
                        resource: "nft".to_string(),
                        reason,
                    })
                }
            }
            Err(e) => Err(e),
        }
    }

    fn derive_dhcp_range(cidr: &str) -> Result<(String, String, String), ChvError> {
        let (ip, prefix_str) = cidr
            .split_once('/')
            .ok_or_else(|| ChvError::InvalidArgument {
                field: "cidr".to_string(),
                reason: format!("invalid CIDR: {}", cidr),
            })?;
        let prefix: u8 = prefix_str.parse().map_err(|_| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid prefix in CIDR: {}", cidr),
        })?;

        if prefix == 0 || prefix > 30 {
            return Err(ChvError::InvalidArgument {
                field: "cidr".to_string(),
                reason: format!(
                    "prefix length /{} is not suitable for DHCP (must be 1-30)",
                    prefix
                ),
            });
        }

        let octets: Vec<&str> = ip.split('.').collect();
        if octets.len() != 4 {
            return Err(ChvError::InvalidArgument {
                field: "cidr".to_string(),
                reason: format!("invalid IP in CIDR: {}", cidr),
            });
        }

        let o0: u8 = octets[0].parse().map_err(|_| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid octet in IP: {}", cidr),
        })?;
        let o1: u8 = octets[1].parse().map_err(|_| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid octet in IP: {}", cidr),
        })?;
        let o2: u8 = octets[2].parse().map_err(|_| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid octet in IP: {}", cidr),
        })?;
        let o3: u8 = octets[3].parse().map_err(|_| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid octet in IP: {}", cidr),
        })?;

        let ip_u32 = u32::from_be_bytes([o0, o1, o2, o3]);
        let mask: u32 = !0u32 << (32 - prefix);
        let network = ip_u32 & mask;
        let broadcast = network | !mask;

        // Compute netmask string from prefix
        let netmask_bytes = mask.to_be_bytes();
        let netmask = format!(
            "{}.{}.{}.{}",
            netmask_bytes[0], netmask_bytes[1], netmask_bytes[2], netmask_bytes[3]
        );

        // DHCP range: skip the first few addresses (network + gateway) and last few (broadcast).
        // For large subnets (>100 hosts), use offset of 50 from each end.
        // For small subnets, start at network+2 (skip network and gateway) and end at broadcast-1.
        let host_count = broadcast - network;
        let offset_start = if host_count > 100 { 50 } else { 2 };
        let offset_end = if host_count > 100 { 50 } else { 1 };

        let range_start_u32 = network + offset_start;
        let range_end_u32 = broadcast - offset_end;

        if range_start_u32 >= range_end_u32 {
            return Err(ChvError::InvalidArgument {
                field: "cidr".to_string(),
                reason: format!(
                    "prefix length /{} results in too few addresses for a DHCP range",
                    prefix
                ),
            });
        }

        let start_bytes = range_start_u32.to_be_bytes();
        let end_bytes = range_end_u32.to_be_bytes();

        let range_start = format!(
            "{}.{}.{}.{}",
            start_bytes[0], start_bytes[1], start_bytes[2], start_bytes[3]
        );
        let range_end = format!(
            "{}.{}.{}.{}",
            end_bytes[0], end_bytes[1], end_bytes[2], end_bytes[3]
        );

        Ok((range_start, range_end, netmask))
    }

    async fn read_pid_file(pid_path: &std::path::Path) -> Option<i32> {
        let pid_str = tokio::fs::read_to_string(pid_path).await.ok()?;
        pid_str.trim().parse::<i32>().ok()
    }

    /// Whether the process with `pid` is alive (`kill -0` semantics).
    async fn process_alive(pid: i32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Bounded wait for a signalled process to actually exit (m8 race):
    /// dnsmasq runs with `bind-interfaces`, so a replacement started
    /// while the old process still holds the listen address fails with
    /// NetworkUnavailable ("address already in use") — after the fabric
    /// re-apply already succeeded. Polls `is_alive` every
    /// [`DNSMASQ_EXIT_POLL_INTERVAL`] until it reports the process gone
    /// or `budget` elapses. Returns `true` when the process exited within
    /// the budget. The closure indirection keeps the wait logic
    /// unit-testable without a real process.
    async fn wait_for_process_exit<F, Fut>(mut is_alive: F, budget: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if !is_alive().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(DNSMASQ_EXIT_POLL_INTERVAL).await;
        }
    }

    async fn is_dnsmasq_running(pid_path: &std::path::Path) -> bool {
        match Self::read_pid_file(pid_path).await {
            Some(pid) => Self::process_alive(pid).await,
            None => false,
        }
    }

    async fn start_dnsmasq(
        network_id: &str,
        bridge_name: &str,
        cidr: &str,
        gateway_ip: &str,
        tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError> {
        let runtime_dir = PathBuf::from("/run/chv/nwd");
        let _ = tokio::fs::create_dir_all(&runtime_dir).await;

        let conf_path = runtime_dir.join(format!("dnsmasq-{}.conf", network_id));
        let hosts_path = runtime_dir.join(format!("dnsmasq-{}.hosts", network_id));
        let pid_path = runtime_dir.join(format!("dnsmasq-{}.pid", network_id));
        let lease_path = runtime_dir.join(format!("dnsmasq-{}.leases", network_id));

        if Self::is_dnsmasq_running(&pid_path).await {
            return Ok(());
        }

        // Create empty hostsfile if not exists
        let _ = tokio::fs::write(&hosts_path, "").await;

        let (range_start, range_end, netmask) = Self::derive_dhcp_range(cidr)?;

        // DHCP option 26 (interface MTU): advertise the tenant MTU on
        // fabric-backed topologies so guests frame correctly across the
        // WireGuard+VXLAN overhead (ADR-021 §3). Omitted when unknown.
        let mtu_option = match tenant_mtu {
            Some(mtu) if mtu > 0 => format!("dhcp-option=26,{}\n", mtu),
            _ => String::new(),
        };

        let config = format!(
            "interface={}\nbind-interfaces\nport=0\ndhcp-range={},{},{},12h\ndhcp-option=3,{}\ndhcp-option=6,1.1.1.1\n{}dhcp-hostsfile={}\nexcept-interface=lo\nno-resolv\n",
            bridge_name,
            range_start,
            range_end,
            netmask,
            gateway_ip,
            mtu_option,
            hosts_path.display()
        );
        tokio::fs::write(&conf_path, config)
            .await
            .map_err(|e| ChvError::Io {
                path: conf_path.to_string_lossy().to_string(),
                source: e,
            })?;

        let dnsmasq_args = Self::dnsmasq_args(&conf_path, &pid_path, &lease_path);
        let out = Command::new("dnsmasq")
            .args(dnsmasq_args)
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "dnsmasq".to_string(),
                source: e,
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ChvError::NetworkUnavailable {
                resource: "dnsmasq".to_string(),
                reason: format!("dnsmasq failed: {}", stderr),
            });
        }

        Ok(())
    }

    fn dnsmasq_args(
        conf_path: &std::path::Path,
        pid_path: &std::path::Path,
        lease_path: &std::path::Path,
    ) -> [String; 3] {
        [
            format!("--conf-file={}", conf_path.display()),
            format!("--pid-file={}", pid_path.display()),
            // dnsmasq's default lease path (/var/lib/misc) is outside the
            // nwd sandbox's writable roots; without this the daemon exits
            // with "cannot open or create lease file: Permission denied"
            // and DHCP is dead (rc.6 blocker B4).
            format!("--dhcp-leasefile={}", lease_path.display()),
        ]
    }

    async fn signal_by_pid_file(pid_path: &std::path::Path, signal: &str) {
        let Ok(pid_str) = tokio::fs::read_to_string(pid_path).await else {
            return;
        };
        let _ = Command::new("kill")
            .args([signal, pid_str.trim()])
            .output()
            .await;
    }

    async fn stop_dnsmasq(network_id: &str) {
        let runtime_dir = PathBuf::from("/run/chv/nwd");
        let pid_path = runtime_dir.join(format!("dnsmasq-{}.pid", network_id));
        let conf_path = runtime_dir.join(format!("dnsmasq-{}.conf", network_id));
        let hosts_path = runtime_dir.join(format!("dnsmasq-{}.hosts", network_id));
        let lease_path = runtime_dir.join(format!("dnsmasq-{}.leases", network_id));

        // SIGTERM, then a bounded wait for the process to actually exit
        // before the pid file is removed (m8 race): dnsmasq uses
        // bind-interfaces, so an immediate restart (the MTU re-assert
        // path) or a fast re-ensure after this delete would fail to bind
        // the listen address while the old process is still tearing down.
        let pid = Self::read_pid_file(&pid_path).await;
        Self::signal_by_pid_file(&pid_path, "-TERM").await;
        if let Some(pid) = pid {
            let exited =
                Self::wait_for_process_exit(|| Self::process_alive(pid), DNSMASQ_EXIT_TERM_BUDGET)
                    .await;
            if !exited {
                warn!(
                    network_id = %network_id,
                    pid,
                    "dnsmasq did not exit within the bounded wait after SIGTERM; sending SIGKILL"
                );
                // Accepted-risk: the pid is not re-validated against the
                // pid file (or via pidfd) before SIGKILL, so a pid reused
                // by an unrelated process inside the <=2s+250ms window
                // could take the kill. The window is bounded and the pid
                // was ours when read; harden with pidfd if this ever
                // bites in practice.
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .output()
                    .await;
                // One final brief wait so a fast re-ensure still finds
                // the listen address free in the common case.
                let _ = Self::wait_for_process_exit(
                    || Self::process_alive(pid),
                    DNSMASQ_EXIT_KILL_BUDGET,
                )
                .await;
            }
        }

        let _ = tokio::fs::remove_file(&pid_path).await;
        let _ = tokio::fs::remove_file(&conf_path).await;
        let _ = tokio::fs::remove_file(&hosts_path).await;
        let _ = tokio::fs::remove_file(&lease_path).await;
    }

    async fn add_dhcp_host(network_id: &str, mac_address: &str, ip_address: &str) {
        let hosts_path = format!("/run/chv/nwd/dnsmasq-{}.hosts", network_id);
        let pid_path = std::path::PathBuf::from(format!("/run/chv/nwd/dnsmasq-{}.pid", network_id));

        let content = tokio::fs::read_to_string(&hosts_path)
            .await
            .unwrap_or_default();
        let entry = format!("{},{}\n", mac_address, ip_address);

        if !content.contains(mac_address) {
            if let Ok(mut file) = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&hosts_path)
                .await
            {
                let _ = file.write_all(entry.as_bytes()).await;
            }
        }

        Self::signal_by_pid_file(&pid_path, "-HUP").await;
    }

    /// Install (idempotently replace) the DNAT rules for one service exposure.
    ///
    /// Deletes any prior rules carrying this exposure's comment marker, then
    /// re-adds the prerouting DNAT and the forward accept. The forward accept
    /// must be evaluated BEFORE the CHV guarded dispatch jumps (otherwise the
    /// flow enters `chv-policy-fwd` default-deny and is dropped), so it is
    /// INSERTED at the head of the forward chain.
    async fn install_exposure_rules(
        &self,
        network_id: &str,
        safe_exposure_id: &str,
        protocol: &str,
        external_port: u32,
        target_ip: &str,
        target_port: u32,
    ) -> Result<(), ChvError> {
        let table = Self::sanitized_nft_table(network_id)?;
        Self::run_nft_idempotent(&["add", "table", "inet", &table]).await?;
        Self::run_nft_idempotent(&[
            "add",
            "chain",
            "inet",
            &table,
            "prerouting",
            "{ type nat hook prerouting priority 0 ; policy accept ; }",
        ])
        .await?;
        Self::run_nft_idempotent(&[
            "add",
            "chain",
            "inet",
            &table,
            "forward",
            "{ type filter hook forward priority filter ; policy accept ; }",
        ])
        .await?;

        // Idempotent replace: drop any prior rules carrying this marker.
        Self::delete_rules_by_comment(&table, "prerouting", safe_exposure_id).await?;
        Self::delete_rules_by_comment(&table, "forward", safe_exposure_id).await?;

        // In an `inet` (dual-stack) table `dnat to` is ambiguous; nft requires
        // `dnat ip to` / `dnat ip6 to` with a bracketed IPv6-address:port, and
        // the forward-accept must use the matching `ip`/`ip6 daddr` expression.
        let (nft_family, dnat_target, daddr_expr) = match target_ip.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(_)) => (
                "ip6".to_string(),
                format!("[{}]:{}", target_ip, target_port),
                "ip6",
            ),
            _ => (
                "ip".to_string(),
                format!("{}:{}", target_ip, target_port),
                "ip",
            ),
        };

        // Install the forward accept BEFORE the prerouting DNAT so a failure
        // here cannot leave a half-applied DNAT rule that redirects traffic
        // into the still-default-deny forward path.
        Self::run_nft(&[
            "insert",
            "rule",
            "inet",
            &table,
            "forward",
            protocol,
            "dport",
            &target_port.to_string(),
            daddr_expr,
            "daddr",
            target_ip,
            "accept",
            "comment",
            &format!("\"{}\"", safe_exposure_id),
        ])
        .await?;
        Self::run_nft(&[
            "add",
            "rule",
            "inet",
            &table,
            "prerouting",
            // Never DNAT loopback. A full bind-interface source guard still
            // requires a declared uplink in the exposure API (tracked); the
            // filter hooks use the CHV-owned interface guards from the firewall
            // path, and any unrelated host port collision on the same host IP is
            // a documented residual limitation of the exposure feature.
            "iifname",
            "!=",
            "lo",
            protocol,
            "dport",
            &external_port.to_string(),
            "dnat",
            &nft_family,
            "to",
            &dnat_target,
            "comment",
            &format!("\"{}\"", safe_exposure_id),
        ])
        .await?;
        Ok(())
    }

    /// Re-assert all stored service exposures after a firewall apply that
    /// rebuilt the `forward` base chain (exposure forward-accept rules were
    /// destroyed by that rebuild). Idempotent per exposure.
    async fn reassert_exposures(&self, network_id: &str) -> Result<(), ChvError> {
        let recs: Vec<ExposureSpec> = self
            .exposures
            .get(network_id)
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default();
        for rec in &recs {
            self.install_exposure_rules(
                network_id,
                &rec.safe_exposure_id,
                &rec.protocol,
                rec.external_port,
                &rec.target_ip,
                rec.target_port,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl NetworkExecutor for LinuxExecutor {
    async fn ensure_topology(&self, spec: &TopologySpec) -> Result<TopologyApplyResult, ChvError> {
        let _guard = self.nft_lock.lock().await;
        info!(
            network_id = %spec.network_id,
            bridge = %spec.bridge_name,
            namespace = %spec.namespace_name,
            "ensuring topology"
        );

        // Fail closed on a fabric plan that cannot be realized (m6):
        // symmetric with the missing-plan error below — a plan with
        // vni == 0 or a non-VXLAN overlay type is never silently skipped,
        // which would desynchronize the datapath from the desired state.
        if spec.fabric.is_some() {
            if spec.vni == 0 {
                return Err(ChvError::InvalidArgument {
                    field: "vni".to_string(),
                    reason: format!(
                        "fabric plan present for network {} but vni is 0; \
                         a fabric overlay requires a nonzero VNI",
                        spec.network_id
                    ),
                });
            }
            if spec.overlay_type != OverlayType::OverlayVxlan as i32 {
                return Err(ChvError::InvalidArgument {
                    field: "overlay_type".to_string(),
                    reason: format!(
                        "fabric plan present for network {} but overlay_type is {} \
                         ({}), not OVERLAY_VXLAN ({})",
                        spec.network_id,
                        spec.overlay_type,
                        OverlayType::try_from(spec.overlay_type)
                            .map(|t| t.as_str_name())
                            .unwrap_or("unknown"),
                        OverlayType::OverlayVxlan as i32
                    ),
                });
            }
        }

        // Bridge
        if !Self::bridge_exists(&spec.bridge_name).await {
            Self::run_ip(&["link", "add", &spec.bridge_name, "type", "bridge"]).await?;
        }
        Self::run_ip(&["link", "set", &spec.bridge_name, "up"]).await?;

        // Assign gateway IP to bridge
        if !spec.gateway_ip.is_empty() && !spec.subnet_cidr.is_empty() {
            let prefix = spec.subnet_cidr.split('/').nth(1).unwrap_or("24");
            if let Err(e) = Self::run_ip(&[
                "addr",
                "add",
                &format!("{}/{}", spec.gateway_ip, prefix),
                "dev",
                &spec.bridge_name,
            ])
            .await
            {
                let reason = e.to_string();
                if !reason.contains("File exists")
                    && !reason.contains("RTNETLINK answers: File exists")
                {
                    tracing::warn!(
                        bridge = %spec.bridge_name,
                        gateway = %spec.gateway_ip,
                        error = %e,
                        "failed to assign gateway IP to bridge"
                    );
                }
            }
        }

        // Stretched-L2 fabric (ADR-021): realize the fabric plan BEFORE
        // dnsmasq starts so the DHCP scope can advertise the authoritative
        // tenant MTU (option 26) computed from the applied plan. Replaces
        // the legacy nolearning VXLAN branch, which was inert (vtep_ip was
        // never configured and the executor failed closed).
        let mut tenant_mtu = None;
        let mut fabric_plan_generation = None;
        let mut binding_generation = None;
        if spec.vni > 0 && spec.overlay_type == OverlayType::OverlayVxlan as i32 {
            let fabric_plan = spec
                .fabric
                .as_ref()
                .ok_or_else(|| ChvError::InvalidArgument {
                    field: "fabric".to_string(),
                    reason: "VXLAN overlay requires a fabric plan".to_string(),
                })?;
            let applied = self
                .apply_fabric_overlay(&spec.network_id, spec.vni, fabric_plan, &spec.bridge_name)
                .await?;
            info!(
                network_id = %spec.network_id,
                vni = spec.vni,
                tenant_mtu = applied.tenant_mtu,
                consumer_veth = %applied.consumer_veth,
                created_fabric = applied.report.created_fabric,
                created_network = applied.report.created_network,
                "fabric overlay applied"
            );
            tenant_mtu = Some(applied.tenant_mtu);
            fabric_plan_generation = Some(applied.plan_generation);
            binding_generation = Some(applied.binding_generation);
        }

        // Start dnsmasq for DHCP
        if !spec.subnet_cidr.is_empty() && !spec.gateway_ip.is_empty() {
            if let Err(e) = Self::start_dnsmasq(
                &spec.network_id,
                &spec.bridge_name,
                &spec.subnet_cidr,
                &spec.gateway_ip,
                tenant_mtu,
            )
            .await
            {
                warn!(error = %e, "failed to start dnsmasq");
            }
        }

        // Namespace
        if !Self::namespace_exists(&spec.namespace_name).await {
            Self::run_ip(&["netns", "add", &spec.namespace_name]).await?;
        }

        let _ = Self::run_nft_quiet(&["add", "table", "inet", &format!("chv-{}", spec.network_id)])
            .await;

        Ok(TopologyApplyResult {
            namespace_handle: spec.namespace_name.clone(),
            bridge_handle: spec.bridge_name.clone(),
            tenant_mtu,
            fabric_plan_generation,
            binding_generation,
        })
    }

    async fn delete_topology(
        &self,
        network_id: &str,
        state: &crate::state::TopologyState,
    ) -> Result<DeleteOutcome, ChvError> {
        let _guard = self.nft_lock.lock().await;
        info!(
            network_id = %network_id,
            bridge = %state.bridge_name,
            namespace = %state.namespace_name,
            "deleting topology"
        );

        // Fabric teardown runs FIRST, in reverse dependency order: the local
        // tenant bridge enslaves the fabric's consumer veth, so the fabric
        // network objects must be removed before local teardown. The shared
        // fabric and the WireGuard key survive (ADR-021 §4).
        //
        // Teardown is fail-open for the fabric half only (m5): a persistent
        // fabric error — or a fabric provider disabled in configuration
        // after the overlay was applied — must not block dnsmasq/netns/
        // bridge/nft cleanup forever. The residue stays visible via the
        // provider's durable ownership journal, and the apply path remains
        // fail-closed. The fabric-half outcome is reported through
        // `DeleteOutcome` so the handler can count the remove metric
        // truthfully (n11) instead of proxying it from this aggregate
        // result.
        let mut fabric_removed = None;
        if state.fabric_plan_generation.is_some() {
            match self.fabric.as_ref() {
                None => {
                    warn!(
                        network_id = %network_id,
                        "fabric provider disabled in nwd configuration; skipping fabric \
                         teardown (residue remains visible via the provider ownership journal)"
                    );
                }
                Some(fabric) => match fabric.remove_network(network_id).await {
                    Ok(()) => fabric_removed = Some(Ok(())),
                    Err(e) => {
                        warn!(
                            network_id = %network_id,
                            error = %e,
                            "fabric overlay removal failed; continuing local topology \
                             teardown (fail-open for teardown only — apply stays fail-closed)"
                        );
                        fabric_removed = Some(Err(e));
                    }
                },
            }
        }

        Self::stop_dnsmasq(network_id).await;

        if Self::namespace_exists(&state.namespace_name).await {
            if let Err(e) = Self::run_ip(&["netns", "del", &state.namespace_name]).await {
                warn!(error = %e, "failed to delete namespace");
            }
        }

        if Self::bridge_exists(&state.bridge_name).await {
            if let Err(e) = Self::run_ip(&["link", "del", "dev", &state.bridge_name]).await {
                warn!(error = %e, "failed to delete bridge");
            }
        }

        if let Ok(table) = Self::sanitized_nft_table(network_id) {
            let _ = Self::run_nft_quiet(&["delete", "table", "inet", &table]).await;
        }
        // Drop remembered service exposures for this network.
        self.exposures.remove(network_id);

        Ok(DeleteOutcome { fabric_removed })
    }

    async fn delete_local_topology_by_derived_names(&self, network_id: &str) {
        // Same local teardown as `delete_topology`, with every name DERIVED
        // from `network_id` because no TopologyState row survives the nwd
        // restart that got us here: namespace `ns-{network_id}` (the
        // executor's own convention, see `attach_vm_nic`), bridge via the
        // shared `bridge_name_for_network` (the same formula the agent
        // used when it ensured the topology), dnsmasq and the nft table
        // already keyed by `network_id`. Existence-gated and warn-only: a
        // network that never materialized on this node is a no-op, and a
        // teardown failure is residue (loud), never an outage. The nft
        // lock mirrors the state-row path so a concurrent firewall apply
        // cannot race the table delete.
        let _guard = self.nft_lock.lock().await;
        info!(
            network_id = %network_id,
            "deleting topology by derived names (no in-memory state row)"
        );

        Self::stop_dnsmasq(network_id).await;

        let namespace_name = format!("ns-{network_id}");
        if Self::namespace_exists(&namespace_name).await {
            if let Err(e) = Self::run_ip(&["netns", "del", &namespace_name]).await {
                warn!(network_id, error = %e, "derived-names teardown: failed to delete namespace");
            }
        }

        let bridge_name = chv_common::bridge_name_for_network(network_id);
        if Self::bridge_exists(&bridge_name).await {
            if let Err(e) = Self::run_ip(&["link", "del", "dev", &bridge_name]).await {
                warn!(network_id, error = %e, "derived-names teardown: failed to delete bridge");
            }
        }

        if let Ok(table) = Self::sanitized_nft_table(network_id) {
            let _ = Self::run_nft_quiet(&["delete", "table", "inet", &table]).await;
        }
        self.exposures.remove(network_id);
    }

    async fn health(
        &self,
        _network_id: &str,
        state: &crate::state::TopologyState,
    ) -> Result<String, ChvError> {
        let bridge_ok = Self::bridge_exists(&state.bridge_name).await;
        let ns_ok = Self::namespace_exists(&state.namespace_name).await;

        if bridge_ok && ns_ok {
            return Ok("healthy".to_string());
        }

        let mut missing = Vec::new();
        if !bridge_ok {
            missing.push("bridge");
        }
        if !ns_ok {
            missing.push("namespace");
        }
        Ok(format!("degraded: missing {}", missing.join(", ")))
    }

    async fn attach_vm_nic(
        &self,
        network_id: &str,
        nic_id: &str,
        _vm_id: &str,
        bridge_name: &str,
        tenant_mtu: Option<u32>,
        mac_address: &str,
        ip_address: &str,
    ) -> Result<(String, String), ChvError> {
        let tap_name = Self::tap_name_for_nic(nic_id);

        // Check if tap already exists; if so, just ensure it's on the right bridge and up.
        let tap_exists = Command::new("ip")
            .args(["link", "show", "dev", &tap_name])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !tap_exists {
            Self::run_ip(&["tuntap", "add", "dev", &tap_name, "mode", "tap"]).await?;
        }
        Self::run_ip(&["link", "set", "dev", &tap_name, "master", bridge_name]).await?;
        Self::run_ip(&["link", "set", "dev", &tap_name, "up"]).await?;

        // Fabric-backed topologies carry the tenant MTU onto the TAP so
        // guest frames fit the WireGuard+VXLAN overhead (ADR-021 §3).
        if let Some(mtu) = tenant_mtu {
            if mtu > 0 {
                let mtu_str = mtu.to_string();
                Self::run_ip(&["link", "set", "dev", &tap_name, "mtu", &mtu_str]).await?;
            }
        }

        Self::add_dhcp_host(network_id, mac_address, ip_address).await;

        info!(network_id = %network_id, nic_id = %nic_id, tap = %tap_name, "attached VM NIC");

        Ok((format!("ns-{}", network_id), tap_name))
    }

    async fn detach_vm_nic(
        &self,
        nic_id: &str,
        ownership: chv_common::AttachmentOwnership,
    ) -> Result<(), ChvError> {
        if ownership.vm_id.is_empty() {
            return Err(ChvError::InvalidArgument {
                field: "vm_id".to_string(),
                reason: "missing vm_id for detach".to_string(),
            });
        }
        let tap_handle = Self::tap_name_for_nic(nic_id);
        let out = Command::new("ip")
            .args(["tuntap", "del", "dev", &tap_handle, "mode", "tap"])
            .output()
            .await
            .map_err(|e| ChvError::Io {
                path: "ip".to_string(),
                source: e,
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("cannot find device") || stderr.contains("No such device") {
                return Ok(());
            }
            return Err(ChvError::NetworkUnavailable {
                resource: "ip".to_string(),
                reason: format!(
                    "ip tuntap del dev {} mode tap failed: {}",
                    tap_handle, stderr
                ),
            });
        }

        info!(tap = %tap_handle, "detached VM NIC");
        Ok(())
    }

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        _policy_version: &str,
        policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError> {
        let _guard = self.nft_lock.lock().await;
        let table = Self::sanitized_nft_table(network_id)?;
        let owned = Self::owned_ifaces_for_bridge(bridge_name)
            .await
            .inspect_err(|_e| {
                metrics::counter!(NWD_NFT_ERRORS_TOTAL, "operation" => "owned_ifaces_for_bridge")
                    .increment(1);
            })?;
        crate::firewall::apply_firewall_rules(&table, &owned, policy_json)
            .await
            .inspect_err(|_e| {
                metrics::counter!(NWD_NFT_ERRORS_TOTAL, "operation" => "apply_firewall")
                    .increment(1);
            })?;
        // The firewall apply rebuilds the `forward` base chain, destroying any
        // service-exposure forward-accept rules; re-assert them inside the
        // CHV boundary so exposed flows are not silently dropped (#227 S3).
        self.reassert_exposures(network_id).await.inspect_err(|_e| {
            metrics::counter!(NWD_NFT_ERRORS_TOTAL, "operation" => "reassert_exposures")
                .increment(1);
        })
    }

    async fn set_nat_policy(
        &self,
        network_id: &str,
        _policy_version: &str,
        policy_json: &[u8],
        bridge_name: &str,
    ) -> Result<(), ChvError> {
        let _guard = self.nft_lock.lock().await;
        let table = Self::sanitized_nft_table(network_id)?;
        let owned = Self::owned_ifaces_for_bridge(bridge_name)
            .await
            .inspect_err(|_e| {
                metrics::counter!(NWD_NFT_ERRORS_TOTAL, "operation" => "owned_ifaces_for_bridge")
                    .increment(1);
            })?;
        crate::firewall::apply_nat_rules(&table, &owned, policy_json)
            .await
            .inspect_err(|_e| {
                metrics::counter!(NWD_NFT_ERRORS_TOTAL, "operation" => "apply_nat").increment(1);
            })
    }

    async fn ensure_dhcp_scope(
        &self,
        network_id: &str,
        cidr: &str,
        range_start: &str,
        range_end: &str,
        dns_servers: &[String],
    ) -> Result<(), ChvError> {
        crate::dhcp::ensure_dhcp_scope(network_id, cidr, range_start, range_end, dns_servers)
            .await
            .inspect_err(|_e| {
                metrics::counter!(NWD_DHCP_ERRORS_TOTAL, "operation" => "ensure_scope")
                    .increment(1);
            })
    }

    async fn ensure_dns_scope(
        &self,
        network_id: &str,
        forwarders: &[&str],
        static_records: &std::collections::HashMap<String, String>,
    ) -> Result<(), ChvError> {
        crate::dns::ensure_dns_scope(network_id, forwarders, static_records).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn expose_service(
        &self,
        network_id: &str,
        exposure_id: &str,
        protocol: &str,
        external_port: u32,
        target_ip: &str,
        target_port: u32,
        _mode: &str,
    ) -> Result<(), ChvError> {
        // Serialize with firewall/NAT applies and topology teardown so a
        // concurrent policy apply cannot flush this exposure's accept rule
        // mid-install.
        let _guard = self.nft_lock.lock().await;
        // Validate protocol to prevent command injection
        const ALLOWED_PROTOCOLS: &[&str] = &["tcp", "udp", "icmp", "sctp"];
        if !ALLOWED_PROTOCOLS.contains(&protocol) {
            return Err(ChvError::InvalidArgument {
                field: "protocol".to_string(),
                reason: format!(
                    "invalid protocol '{}': must be one of tcp, udp, icmp, sctp",
                    protocol
                ),
            });
        }

        // Validate target_ip to prevent command injection
        if target_ip.parse::<std::net::IpAddr>().is_err() {
            return Err(ChvError::InvalidArgument {
                field: "target_ip".to_string(),
                reason: format!("invalid IP address: '{}'", target_ip),
            });
        }

        let safe_exposure_id = Self::sanitize_id(exposure_id)?;
        self.install_exposure_rules(
            network_id,
            &safe_exposure_id,
            protocol,
            external_port,
            target_ip,
            target_port,
        )
        .await?;
        // Record so the exposure can be re-asserted after a firewall apply
        // rebuilds the forward base chain. Replacing an existing exposure_id
        // replaces its record (no unbounded growth).
        self.exposures
            .entry(network_id.to_string())
            .or_default()
            .retain(|r| r.safe_exposure_id != safe_exposure_id);
        self.exposures
            .entry(network_id.to_string())
            .or_default()
            .push(ExposureSpec {
                safe_exposure_id: safe_exposure_id.clone(),
                protocol: protocol.to_string(),
                external_port,
                target_ip: target_ip.to_string(),
                target_port,
            });
        info!(network_id = %network_id, exposure_id = %exposure_id, "service exposed via DNAT");
        Ok(())
    }

    async fn withdraw_service_exposure(
        &self,
        network_id: &str,
        exposure_id: &str,
    ) -> Result<(), ChvError> {
        let _guard = self.nft_lock.lock().await;
        let table = Self::sanitized_nft_table(network_id)?;
        let safe_exposure_id = Self::sanitize_id(exposure_id)?;
        Self::delete_rules_by_comment(&table, "prerouting", &safe_exposure_id).await?;
        Self::delete_rules_by_comment(&table, "forward", &safe_exposure_id).await?;
        {
            let removed_all = if let Some(mut entry) = self.exposures.get_mut(network_id) {
                let before = entry.len();
                entry.retain(|r| r.safe_exposure_id != safe_exposure_id);
                before > 0 && entry.is_empty()
            } else {
                false
            };
            // Drop the now-empty key so no stale empty entry lingers.
            if removed_all {
                self.exposures.remove(network_id);
            }
        }
        info!(network_id = %network_id, exposure_id = %exposure_id, "service exposure withdrawn");
        Ok(())
    }

    // --- Gratuitous ARP (ADR-021: flushes stale MAC/ARP caches after a
    // migration; the fabric's kernel MAC learning repopulates them) ---

    async fn send_gratuitous_arp(
        &self,
        namespace: &str,
        bridge_name: &str,
        vm_ip: &str,
    ) -> Result<(), ChvError> {
        let out = Self::run_cmd_netns_output(
            namespace,
            "arping",
            &["-U", "-c", "3", "-I", bridge_name, vm_ip],
        )
        .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!(namespace = %namespace, vm_ip = %vm_ip, error = %stderr, "gratuitous ARP failed");
        }
        Ok(())
    }

    // --- Stretched-L2 fabric implementations (ADR-021) ---

    async fn apply_fabric_overlay(
        &self,
        network_id: &str,
        vni: u32,
        plan: &FabricPlan,
        bridge_name: &str,
    ) -> Result<AppliedFabric, ChvError> {
        let fabric = self.fabric_handle()?;
        let applied = fabric.apply(network_id, vni, plan).await?;
        // Graft the fabric consumer veth into the tenant bridge and carry
        // the tenant MTU onto the bridge. Any failure here is surfaced as
        // NetworkUnavailable by run_ip (fail closed).
        for args in
            Self::fabric_attach_commands(&applied.consumer_veth, bridge_name, applied.tenant_mtu)
        {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            Self::run_ip(&refs).await?;
        }
        Ok(applied)
    }

    async fn remove_fabric_overlay(&self, network_id: &str) -> Result<(), ChvError> {
        let fabric = self.fabric_handle()?;
        fabric.remove_network(network_id).await
    }

    async fn fabric_owned(&self, network_id: &str) -> Result<FabricOwnership, ChvError> {
        match self.fabric.as_ref() {
            // Provider disabled in configuration: this process holds (and
            // can observe) no fabric state — the caller must warn instead
            // of treating this as "not owned" (silent no-op).
            None => Ok(FabricOwnership::ProviderDisabled),
            Some(fabric) => {
                if fabric.fabric_owned(network_id).await? {
                    Ok(FabricOwnership::Owned)
                } else {
                    Ok(FabricOwnership::NotOwned)
                }
            }
        }
    }

    async fn fabric_identity(&self) -> Result<FabricIdentity, ChvError> {
        let fabric = self.fabric_handle()?;
        fabric.identity().await
    }

    async fn fabric_overlay_status(&self, network_id: &str) -> Result<OverlayStatusInfo, ChvError> {
        let fabric = self.fabric_handle()?;
        fabric.overlay_status(network_id).await
    }

    async fn reassert_tenant_mtu(
        &self,
        network_id: &str,
        bridge_name: &str,
        subnet_cidr: &str,
        gateway_ip: &str,
        tenant_mtu: Option<u32>,
    ) -> Result<(), ChvError> {
        // Serialize with topology create/delete so the port enumeration
        // cannot race a concurrent enslavement.
        let _guard = self.nft_lock.lock().await;
        // `None` resets a previously fabric-backed bridge to the
        // bridge-only default (m7 residue): a fresh bridge-only ensure
        // never sets an MTU, so the kernel default for a new bridge is
        // the authoritative value.
        let effective_mtu = tenant_mtu.unwrap_or(BRIDGE_ONLY_DEFAULT_MTU);
        match tenant_mtu {
            Some(tenant_mtu) => info!(
                network_id = %network_id,
                bridge = %bridge_name,
                tenant_mtu,
                "tenant MTU changed; re-asserting bridge/port MTUs and restarting dnsmasq"
            ),
            None => info!(
                network_id = %network_id,
                bridge = %bridge_name,
                default_mtu = effective_mtu,
                "fabric overlay removed; resetting bridge/port MTUs to the bridge-only \
                 default and restarting dnsmasq without DHCP option 26"
            ),
        }

        // (a) Re-assert the bridge MTU and the MTU of every port currently
        // enslaved to it (TAPs and the fabric consumer veth alike).
        let owned = Self::owned_ifaces_for_bridge(bridge_name).await?;
        for args in Self::port_mtu_commands(&owned, effective_mtu) {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            Self::run_ip(&refs).await?;
        }

        // (b) Restart dnsmasq so the rewritten config takes effect: with
        // `Some(mtu)` the new DHCP option 26 advertises it, with `None`
        // the option is dropped entirely (bridge-only default).
        // start_dnsmasq early-returns while the old instance is still
        // running; stop_dnsmasq bounded-waits for the old process to exit
        // so the restart cannot fail on "address already in use" (m8).
        if !subnet_cidr.is_empty() && !gateway_ip.is_empty() {
            Self::stop_dnsmasq(network_id).await;
            Self::start_dnsmasq(network_id, bridge_name, subnet_cidr, gateway_ip, tenant_mtu)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_executor_implements_network_executor() {
        let _executor = LinuxExecutor::new(std::env::temp_dir());
        // If this compiles, the trait is fully implemented.
    }

    #[test]
    fn dnsmasq_argv_uses_a_writable_lease_file() {
        // rc.6 B4: dnsmasq's default lease file (/var/lib/misc) is outside
        // the nwd sandbox's writable roots, so the daemon exited with
        // "Permission denied" and DHCP never served a lease.
        let conf = std::path::Path::new("/run/chv/nwd/dnsmasq-net1.conf");
        let pid = std::path::Path::new("/run/chv/nwd/dnsmasq-net1.pid");
        let lease = std::path::Path::new("/run/chv/nwd/dnsmasq-net1.leases");
        let args = LinuxExecutor::dnsmasq_args(conf, pid, lease);
        assert_eq!(args[0], "--conf-file=/run/chv/nwd/dnsmasq-net1.conf");
        assert_eq!(args[1], "--pid-file=/run/chv/nwd/dnsmasq-net1.pid");
        assert_eq!(
            args[2], "--dhcp-leasefile=/run/chv/nwd/dnsmasq-net1.leases",
            "the lease file must live under the nwd ReadWritePaths root"
        );
    }

    #[test]
    fn fabric_attach_commands_enslave_and_set_mtu() {
        let commands = LinuxExecutor::fabric_attach_commands("chv-c-0123abcd", "br-net-1", 1380);
        assert_eq!(
            commands,
            vec![
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "chv-c-0123abcd".to_string(),
                    "master".to_string(),
                    "br-net-1".to_string()
                ],
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "br-net-1".to_string(),
                    "mtu".to_string(),
                    "1380".to_string()
                ],
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "chv-c-0123abcd".to_string(),
                    "mtu".to_string(),
                    "1380".to_string()
                ],
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "chv-c-0123abcd".to_string(),
                    "up".to_string()
                ],
            ],
            "the fabric consumer veth must be enslaved to the tenant bridge, \
             carry the tenant MTU, and be brought up"
        );
    }

    #[tokio::test]
    async fn fabric_rpcs_fail_closed_when_provider_disabled() {
        let executor = LinuxExecutor::new(std::env::temp_dir());

        let plan = FabricPlan::default();
        let err = executor
            .apply_fabric_overlay("net-1", 100, &plan, "br-net-1")
            .await
            .expect_err("fabric apply must fail closed when disabled");
        match err {
            ChvError::InvalidArgument { field, reason } => {
                assert_eq!(field, "fabric");
                assert!(reason.contains("disabled"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }

        let err = executor
            .remove_fabric_overlay("net-1")
            .await
            .expect_err("fabric remove must fail closed when disabled");
        assert!(matches!(err, ChvError::InvalidArgument { .. }));

        let err = executor
            .fabric_identity()
            .await
            .expect_err("fabric identity must fail closed when disabled");
        assert!(matches!(err, ChvError::InvalidArgument { .. }));
    }

    #[test]
    fn nft_table_generation() {
        assert_eq!(
            LinuxExecutor::sanitized_nft_table("net1").unwrap(),
            "chv-net1"
        );
    }

    #[test]
    fn sanitize_id_rejects_bad_chars() {
        assert!(LinuxExecutor::sanitize_id("valid_id-123.abc").is_ok());
        assert!(LinuxExecutor::sanitize_id("net1").is_ok());
        assert!(LinuxExecutor::sanitize_id("").is_err());
        assert!(LinuxExecutor::sanitize_id("bad;id").is_err());
        assert!(LinuxExecutor::sanitize_id("bad id").is_err());
        assert!(LinuxExecutor::sanitize_id("bad\"id").is_err());
        assert!(LinuxExecutor::sanitize_id("bad'id").is_err());
        assert!(LinuxExecutor::sanitize_id("bad/id").is_err());
    }

    #[test]
    fn delete_rules_by_comment_line_extraction() {
        // Simulate the parsing logic inline to avoid async test infrastructure
        let sample = r#"
        tcp dport 80 dnat to 10.0.0.2:80 comment "exp-1" handle 10
        tcp dport 443 dnat to 10.0.0.2:443 comment "exp-2" handle 20
        "#;
        let comment = "exp-1";
        let target = format!("comment \"{}\"", comment);
        let mut found_handle = None;
        for line in sample.lines() {
            if line.contains(&target) {
                if let Some(idx) = line.rfind(" handle ") {
                    let handle = line[idx + 8..].split_whitespace().next().unwrap_or("");
                    if !handle.is_empty() {
                        found_handle = Some(handle.to_string());
                    }
                }
            }
        }
        assert_eq!(found_handle, Some("10".to_string()));
    }

    #[test]
    fn port_mtu_commands_cover_bridge_and_every_enslaved_port() {
        let commands = LinuxExecutor::port_mtu_commands(
            &[
                "br-net-1".to_string(),
                "chv-c-0123abcd".to_string(),
                "tap-12ab".to_string(),
            ],
            1400,
        );
        assert_eq!(
            commands,
            vec![
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "dev".to_string(),
                    "br-net-1".to_string(),
                    "mtu".to_string(),
                    "1400".to_string(),
                ],
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "dev".to_string(),
                    "chv-c-0123abcd".to_string(),
                    "mtu".to_string(),
                    "1400".to_string(),
                ],
                vec![
                    "link".to_string(),
                    "set".to_string(),
                    "dev".to_string(),
                    "tap-12ab".to_string(),
                    "mtu".to_string(),
                    "1400".to_string(),
                ],
            ],
            "the MTU must be re-asserted on the bridge and every enslaved port"
        );
    }

    #[test]
    fn tap_name_is_stable_and_linux_safe_length() {
        let nic_id = "95f4f899-58b9-44b6-95f5-0f35a2e590a6-default-network";
        let a = LinuxExecutor::tap_name_for_nic(nic_id);
        let b = LinuxExecutor::tap_name_for_nic(nic_id);
        assert_eq!(a, b);
        assert!(a.len() <= 15, "tap name exceeds Linux IFNAMSIZ: {}", a);
        assert!(a.starts_with("tap-"));
    }

    // ---- fabric plan present but unrealizable (m6) -------------------------

    fn vxlan_spec_with_fabric(vni: u32, overlay_type: i32) -> TopologySpec {
        TopologySpec {
            network_id: "net-m6".to_string(),
            tenant_id: "t1".to_string(),
            bridge_name: "br-m6".to_string(),
            namespace_name: "ns-m6".to_string(),
            subnet_cidr: "10.0.60.0/24".to_string(),
            gateway_ip: "10.0.60.1".to_string(),
            options: Default::default(),
            vni,
            vtep_endpoints: vec![],
            overlay_type,
            fabric: Some(FabricPlan::default()),
        }
    }

    #[tokio::test]
    async fn fabric_plan_with_zero_vni_is_rejected_fail_closed() {
        let executor = LinuxExecutor::new(std::env::temp_dir());
        // Validation happens before any host command, so this is safe to
        // run unprivileged.
        let err = executor
            .ensure_topology(&vxlan_spec_with_fabric(0, OverlayType::OverlayVxlan as i32))
            .await
            .expect_err("fabric plan with vni 0 must be rejected");
        match err {
            ChvError::InvalidArgument { field, reason } => {
                assert_eq!(field, "vni");
                assert!(reason.contains("vni is 0"), "got: {reason}");
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn fabric_plan_with_non_vxlan_overlay_type_is_rejected_fail_closed() {
        let executor = LinuxExecutor::new(std::env::temp_dir());
        let err = executor
            .ensure_topology(&vxlan_spec_with_fabric(
                100,
                OverlayType::OverlayNone as i32,
            ))
            .await
            .expect_err("fabric plan with a non-VXLAN overlay type must be rejected");
        match err {
            ChvError::InvalidArgument { field, reason } => {
                assert_eq!(field, "overlay_type");
                assert!(reason.contains("OVERLAY_VXLAN"), "got: {reason}");
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    // ---- fabric teardown is fail-open in the delete path (m5) --------------

    /// Fabric handle whose every operation fails; used to prove the delete
    /// path completes local teardown despite persistent fabric errors.
    struct FailingFabricHandle;

    #[async_trait]
    impl crate::fabric::FabricHandle for FailingFabricHandle {
        async fn apply(
            &self,
            _network_id: &str,
            _vni: u32,
            _plan: &FabricPlan,
        ) -> Result<AppliedFabric, ChvError> {
            Err(ChvError::Internal {
                reason: "fabric apply deliberately failing".to_string(),
            })
        }

        async fn remove_network(&self, _network_id: &str) -> Result<(), ChvError> {
            Err(ChvError::Internal {
                reason: "fabric remove deliberately failing".to_string(),
            })
        }

        async fn identity(&self) -> Result<FabricIdentity, ChvError> {
            Err(ChvError::Internal {
                reason: "fabric identity deliberately failing".to_string(),
            })
        }

        async fn consumer_veth(&self, _network_id: &str) -> Result<String, ChvError> {
            Err(ChvError::Internal {
                reason: "fabric consumer veth deliberately failing".to_string(),
            })
        }

        async fn fabric_owned(&self, _network_id: &str) -> Result<bool, ChvError> {
            Err(ChvError::Internal {
                reason: "fabric ownership deliberately failing".to_string(),
            })
        }

        async fn overlay_status(&self, _network_id: &str) -> Result<OverlayStatusInfo, ChvError> {
            Err(ChvError::Internal {
                reason: "fabric status deliberately failing".to_string(),
            })
        }
    }

    fn fabric_backed_state() -> crate::state::TopologyState {
        crate::state::TopologyState {
            network_id: "net-m5".to_string(),
            tenant_id: "t1".to_string(),
            bridge_name: "br-m5-nonexistent".to_string(),
            namespace_name: "ns-m5-nonexistent".to_string(),
            subnet_cidr: "10.0.61.0/24".to_string(),
            gateway_ip: "10.0.61.1".to_string(),
            runtime_status: "ensured".to_string(),
            vni: Some(100),
            tenant_mtu: Some(1380),
            fabric_plan_generation: Some(1),
            binding_generation: Some(1),
        }
    }

    #[tokio::test]
    async fn delete_topology_continues_local_teardown_when_fabric_removal_fails() {
        let executor =
            LinuxExecutor::new(std::env::temp_dir()).with_fabric(Arc::new(FailingFabricHandle));
        // Local teardown targets objects that do not exist on this host, so
        // every local step is a no-op — the assertion is that the fabric
        // failure does NOT abort the delete (previously it failed forever).
        let outcome = executor
            .delete_topology("net-m5", &fabric_backed_state())
            .await
            .expect("delete must succeed despite persistent fabric errors");
        // The fabric-half failure must be reported truthfully (n11) so the
        // handler can count the remove metric as a failure.
        assert!(
            matches!(outcome.fabric_removed, Some(Err(_))),
            "a failed fabric removal must surface in the DeleteOutcome, got {:?}",
            outcome.fabric_removed
        );
    }

    #[tokio::test]
    async fn delete_topology_skips_fabric_removal_when_provider_disabled() {
        let executor = LinuxExecutor::new(std::env::temp_dir());
        let outcome = executor
            .delete_topology("net-m5", &fabric_backed_state())
            .await
            .expect("delete must succeed when the fabric provider is disabled");
        // No teardown was attempted (the provider cannot even be asked), so
        // there is no removal outcome to count.
        assert!(outcome.fabric_removed.is_none());
    }

    #[tokio::test]
    async fn fabric_owned_reports_provider_disabled_when_disabled() {
        let executor = LinuxExecutor::new(std::env::temp_dir());
        assert_eq!(
            executor
                .fabric_owned("net-1")
                .await
                .expect("ownership lookup must not fail when disabled"),
            FabricOwnership::ProviderDisabled,
            "a disabled provider cannot observe ownership — the caller must warn, \
             not treat it as not-owned"
        );
    }

    // ---- bounded dnsmasq exit wait (m8 race) --------------------------------

    #[tokio::test]
    async fn wait_for_process_exit_returns_true_once_the_checker_reports_gone() {
        // The "process" reports alive for the first 3 probes, then exits.
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = probes.clone();
        let exited = LinuxExecutor::wait_for_process_exit(
            move || {
                let p = seen.clone();
                async move { p.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 }
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(
            exited,
            "the wait must succeed once the checker reports gone"
        );
        assert!(
            probes.load(std::sync::atomic::Ordering::Relaxed) >= 4,
            "the checker must have been polled past the last alive report"
        );
    }

    #[tokio::test]
    async fn wait_for_process_exit_gives_up_at_the_budget_when_never_exiting() {
        let started = std::time::Instant::now();
        let exited =
            LinuxExecutor::wait_for_process_exit(|| async { true }, Duration::from_millis(50))
                .await;
        assert!(
            !exited,
            "a process that never exits must exhaust the budget, not hang"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait must give up at (not far past) the budget, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn dnsmasq_args_use_equals_form_required_by_dnsmasq() {
        let args = LinuxExecutor::dnsmasq_args(
            std::path::Path::new("/run/chv/nwd/dnsmasq-net.conf"),
            std::path::Path::new("/run/chv/nwd/dnsmasq-net.pid"),
            std::path::Path::new("/run/chv/nwd/dnsmasq-net.leases"),
        );

        assert_eq!(
            args,
            [
                "--conf-file=/run/chv/nwd/dnsmasq-net.conf".to_string(),
                "--pid-file=/run/chv/nwd/dnsmasq-net.pid".to_string(),
                "--dhcp-leasefile=/run/chv/nwd/dnsmasq-net.leases".to_string(),
            ]
        );
    }

    #[test]
    fn parser_extracts_enslaved_member_names_not_ifindexes() {
        // Real `ip link show master <br>` output shapes (iproute2 6.x).
        // Field 0 is the ifindex (`355:`); the member name is field 1,
        // with an optional veth `@peer` suffix and a trailing colon.
        let veth_line =
            "355: vA@vB: <BROADCAST,MULTICAST,M-DOWN> mtu 1500 qdisc noop master brnet state DOWN mode DEFAULT group default qlen 1000";
        assert_eq!(
            LinuxExecutor::parse_ip_link_master_line(veth_line),
            Some("vA".to_string())
        );

        let tap_line =
            "388: tap-12ab: <BROADCAST,MULTICAST> mtu 1500 qdisc noop master brnet state UP mode DEFAULT group default qlen 1000";
        assert_eq!(
            LinuxExecutor::parse_ip_link_master_line(tap_line),
            Some("tap-12ab".to_string())
        );

        // Blank / non-member lines must not yield a member name.
        assert_eq!(LinuxExecutor::parse_ip_link_master_line(""), None);
        assert_eq!(LinuxExecutor::parse_ip_link_master_line("    "), None);

        // Continuation lines inside `ip link show` output must not be parsed as
        // members: their tokens could widen the CHV-owned guard set to a real
        // host interface (round-4 finding).
        let continuation_mac =
            "    link/ether 72:6a:73:3d:a7:9f brd ff:ff:ff:ff:ff:ff permaddr 72:6a:73:3d:a7:9f";
        assert_eq!(
            LinuxExecutor::parse_ip_link_master_line(continuation_mac),
            None
        );
        let continuation_altname = "    altname enp0s18";
        assert_eq!(
            LinuxExecutor::parse_ip_link_master_line(continuation_altname),
            None
        );
        let continuation_inet = "    inet 10.200.1.1/24 brd 10.200.1.255 scope global br0";
        assert_eq!(
            LinuxExecutor::parse_ip_link_master_line(continuation_inet),
            None
        );
    }

    /// End-to-end proof that `owned_ifaces_for_bridge` returns the bridge plus
    /// its real enslaved members (guards the B1 parser regression). Requires
    /// root + `ip`; skipped on CI, run against a real host via:
    ///   sudo the built lib test binary -- --ignored --exact \
    ///     executor::tests::owned_ifaces_resolves_bridge_and_enslaved_members
    #[tokio::test]
    #[ignore = "requires root + iproute2"]
    async fn owned_ifaces_resolves_bridge_and_enslaved_members() {
        use std::process::Command as StdCommand;

        fn sh(args: &[&str]) {
            let status = StdCommand::new("ip")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("spawn ip");
            assert!(status.success(), "ip {:?} failed with {:?}", args, status);
        }

        // Collision-averse, IFNAMSIZ-safe run suffix (16-bit pid + 16-bit
        // sub-second clock), so a leaked resource from a prior crashed run
        // cannot collide with ours.
        let u = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            format!("{:04x}{:04x}", std::process::id() & 0xffff, nanos & 0xffff)
        };
        let br = format!("brt{u}");
        let veth_name = format!("vt{u}");
        let peer_name = format!("vtx{u}");
        assert!(br.len() <= 15 && veth_name.len() <= 15 && peer_name.len() <= 15);

        // Best-effort cleanup of the bridge AND the veth pair on panic/success,
        // so ensl ports do not leak. Bound here, before any setup command, so a
        // mid-setup panic still unwinds through it.
        struct Cleanup {
            links: Vec<String>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for link in self.links.iter().rev() {
                    let _ = StdCommand::new("ip")
                        .args(["link", "del", "dev", link])
                        .status();
                }
            }
        }
        let _cleanup = Cleanup {
            links: vec![br.clone(), veth_name.clone()],
        };

        sh(&["link", "add", &br, "type", "bridge"]);
        sh(&[
            "link", "add", &veth_name, "type", "veth", "peer", "name", &peer_name,
        ]);
        sh(&["link", "set", &veth_name, "master", &br]);

        let owned = LinuxExecutor::owned_ifaces_for_bridge(&br).await.unwrap();
        // The owned set must be EXACTLY the bridge + the enslaved veth (sorted):
        // any continuation-line token (e.g. `altname`, MAC octets) would make
        // this fail on a real host, proving the parser never widens the
        // CHV-owned guard set (round-4 finding, #227 host-safety boundary).
        let mut expected = vec![br.clone(), veth_name.clone()];
        expected.sort();
        assert_eq!(owned, expected, "owned set must be exactly {{br, veth}}");
    }

    #[tokio::test]
    async fn owned_ifaces_fails_closed_when_bridge_missing() {
        let err = LinuxExecutor::owned_ifaces_for_bridge("definitely-not-a-bridge-xyz")
            .await
            .unwrap_err();
        match err {
            ChvError::NotFound { resource, .. } => {
                assert_eq!(resource, "bridge");
            }
            other => panic!("expected NotFound, got {:?}", other),
        }
    }
}
