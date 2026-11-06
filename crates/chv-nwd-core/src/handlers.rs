use crate::ebpf::{self, EbpfManager};
use crate::executor::{FabricOwnership, NetworkExecutor, OverlayStatusInfo, TopologyApplyResult};
use crate::policy_store::{PolicyStore, StoredPolicies};
use crate::state::{TopologyState, TopologyTable};
use crate::topology_store::TopologyStore;
use chv_errors::ChvError;
use chv_nwd_api::chv_nwd_api as proto;
use chv_observability::{operation_span, Metrics};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

/// Last successfully applied CHV firewall/NAT policy for a network, used to
/// re-scope the CHV-owned interface guard set when a VM NIC attaches so the
/// new interface is covered by CHV default-deny.
#[derive(Clone, Default)]
struct AppliedPolicies {
    firewall: Option<Vec<u8>>,
    firewall_version: Option<String>,
    nat: Option<Vec<u8>>,
    nat_version: Option<String>,
}

pub struct NetworkServiceImpl<E: NetworkExecutor> {
    executor: Arc<E>,
    topologies: Arc<TopologyTable>,
    metrics: Arc<Metrics>,
    security_policies: Arc<DashMap<String, proto::SecurityPolicy>>,
    rate_limit_policies: Arc<DashMap<String, proto::RateLimitPolicy>>,
    policy_state: Arc<DashMap<String, AppliedPolicies>>,
    ebpf: Arc<dyn EbpfManager>,
    /// Counter tracking eBPF program load failures.
    ebpf_load_failures: Arc<AtomicU32>,
    /// Durable bridge-only topology journal (rc.8 blocker B9). `None` in unit
    /// tests and in any composition that has not opted in; the production
    /// daemon always wires it (see `cmd/chv-nwd/src/main.rs`).
    topology_store: Option<Arc<TopologyStore>>,
    /// Durable firewall/NAT policy journal (rc.8 B9 follow-up). Re-applied on
    /// recovery so a restart or reboot restores the interface guard set
    /// instead of leaving attached guests outside default-deny.
    policy_store: Option<Arc<PolicyStore>>,
    /// Serializes policy-journal updates with the in-memory `policy_state`
    /// mutation so concurrent firewall/NAT RPCs cannot persist a half pair.
    policy_persist: AsyncMutex<()>,
}

impl<E: NetworkExecutor> NetworkServiceImpl<E> {
    pub fn new(executor: Arc<E>, topologies: Arc<TopologyTable>, metrics: Arc<Metrics>) -> Self {
        Self {
            executor,
            topologies,
            metrics,
            security_policies: Arc::new(DashMap::new()),
            rate_limit_policies: Arc::new(DashMap::new()),
            policy_state: Arc::new(DashMap::new()),
            ebpf: Arc::new(ebpf::NoopEbpfManager),
            ebpf_load_failures: Arc::new(AtomicU32::new(0)),
            topology_store: None,
            policy_store: None,
            policy_persist: AsyncMutex::new(()),
        }
    }

    /// Opt into the durable bridge-only topology journal. Production wiring
    /// calls this once before serving; tests that do not care leave it unset.
    pub fn set_topology_store(&mut self, store: Arc<TopologyStore>) {
        self.topology_store = Some(store);
    }

    /// Opt into the durable policy journal (rc.8 B9 follow-up). Production
    /// wiring calls this alongside [`Self::set_topology_store`].
    pub fn set_policy_store(&mut self, store: Arc<PolicyStore>) {
        self.policy_store = Some(store);
    }

    /// Persist the applied bridge-only spec, or drop a stale bridge-only entry
    /// when the topology is now fabric-backed. Fabric plans are deliberately
    /// not journaled: the stretched-L2 provider owns its durable surface and
    /// claiming recovery here would be false.
    async fn journal_topology(&self, spec: &proto::TopologySpec) -> Result<(), ChvError> {
        let Some(store) = self.topology_store.as_ref() else {
            return Ok(());
        };
        if spec.fabric.is_some() {
            store.remove(&spec.network_id).await
        } else {
            store.persist(spec).await
        }
    }

    async fn forget_topology(&self, network_id: &str) -> Result<(), ChvError> {
        let Some(store) = self.topology_store.as_ref() else {
            return Ok(());
        };
        store.remove(network_id).await
    }

    /// Drop the durable policy for a network. A failure fails the delete
    /// closed: a stale policy re-applied after a restart against a re-created
    /// network with the same id would be the #227 S5 residue class.
    async fn forget_policies(&self, network_id: &str) -> Result<(), ChvError> {
        let Some(store) = self.policy_store.as_ref() else {
            return Ok(());
        };
        store.remove(network_id).await
    }

    /// Record the requested firewall half durably before the executor apply.
    /// Mutating the in-memory pair and taking the persistence snapshot under
    /// one lock keeps a concurrent NAT RPC from persisting a half pair. The
    /// in-memory record is kept even when the journal write fails, so a later
    /// NIC attach still re-scopes; the RPC itself reports the write failure.
    async fn record_firewall_policy(
        &self,
        network_id: &str,
        policy_json: &[u8],
        policy_version: &str,
    ) -> Result<(), ChvError> {
        // Hold the lock across the persist: releasing it before the write
        // would let a concurrent NAT snapshot land first and then be
        // overwritten by this older firewall-only snapshot.
        let _guard = self.policy_persist.lock().await;
        // Re-verify ownership under the lock: a concurrent delete may have
        // removed the topology between the handler's check and this call.
        // Persisting now would leave a durable policy for a deleted network,
        // which a later re-create could inherit after a restart (#227 S5).
        if self.topologies.get(network_id).is_none() {
            return Err(ChvError::NotFound {
                resource: "topology".to_string(),
                id: network_id.to_string(),
            });
        }
        let snapshot = {
            let mut applied = self.policy_state.entry(network_id.to_string()).or_default();
            applied.firewall = Some(policy_json.to_vec());
            applied.firewall_version = Some(policy_version.to_string());
            StoredPolicies {
                firewall: applied.firewall.clone(),
                firewall_version: applied.firewall_version.clone(),
                nat: applied.nat.clone(),
                nat_version: applied.nat_version.clone(),
            }
        };
        match self.policy_store.as_ref() {
            Some(store) => store.persist(network_id, &snapshot).await,
            None => Ok(()),
        }
    }

    /// Record the requested NAT half durably (see `record_firewall_policy`).
    async fn record_nat_policy(
        &self,
        network_id: &str,
        policy_json: &[u8],
        policy_version: &str,
    ) -> Result<(), ChvError> {
        let _guard = self.policy_persist.lock().await;
        // Re-verify ownership under the lock: a concurrent delete may have
        // removed the topology between the handler's check and this call.
        // Persisting now would leave a durable policy for a deleted network,
        // which a later re-create could inherit after a restart (#227 S5).
        if self.topologies.get(network_id).is_none() {
            return Err(ChvError::NotFound {
                resource: "topology".to_string(),
                id: network_id.to_string(),
            });
        }
        let snapshot = {
            let mut applied = self.policy_state.entry(network_id.to_string()).or_default();
            applied.nat = Some(policy_json.to_vec());
            applied.nat_version = Some(policy_version.to_string());
            StoredPolicies {
                firewall: applied.firewall.clone(),
                firewall_version: applied.firewall_version.clone(),
                nat: applied.nat.clone(),
                nat_version: applied.nat_version.clone(),
            }
        };
        match self.policy_store.as_ref() {
            Some(store) => store.persist(network_id, &snapshot).await,
            None => Ok(()),
        }
    }

    /// Heal the durable entry on the idempotent ensure path. Persist only when
    /// the journal is missing or differs from the applied spec: core-native
    /// re-ensures every network on each reconciliation tick, and an
    /// unconditional rewrite would fsync the journal every ~10 s per network.
    async fn repair_topology(&self, spec: &proto::TopologySpec) -> Result<(), ChvError> {
        let Some(store) = self.topology_store.as_ref() else {
            return Ok(());
        };
        if spec.fabric.is_some() {
            return store.remove(&spec.network_id).await;
        }
        match store.load(&spec.network_id).await {
            Ok(Some(journaled)) if &journaled == spec => Ok(()),
            Ok(_) => store.persist(spec).await,
            Err(e) => {
                // A corrupt entry is healed by the authoritative applied
                // spec; an I/O failure resurfaces on the persist below.
                warn!(
                    network_id = %spec.network_id,
                    error = %e,
                    "topology journal entry unreadable on the idempotent ensure path; rewriting it"
                );
                store.persist(spec).await
            }
        }
    }

    /// Re-apply every journaled bridge-only topology before the server starts
    /// accepting RPCs (rc.8 blocker B9). The in-memory [`TopologyTable`] is
    /// rebuilt from the journal, so a later `SetFirewallPolicy` finds the
    /// topology instead of failing `NOT_FOUND`.
    ///
    /// Every entry is attempted even when one fails; the first failure still
    /// aborts startup, because serving with a partial table is exactly the
    /// silent-degradation failure this journal exists to eliminate.
    pub async fn recover_topologies(&self) -> Result<(), ChvError> {
        let Some(store) = self.topology_store.as_ref() else {
            return Ok(());
        };
        let specs = store.load_all().await?;
        if specs.is_empty() {
            return Ok(());
        }
        info!(
            count = specs.len(),
            dir = %store.dir().display(),
            "recovering journaled network topologies"
        );

        let mut failures = Vec::new();
        for spec in specs {
            if spec.fabric.is_some() {
                warn!(
                    network_id = %spec.network_id,
                    "journaled fabric topology is not recovered; the stretched-L2 \
                     provider owns that durable surface"
                );
                continue;
            }
            match self.executor.ensure_topology(&spec).await {
                Ok(applied) => {
                    let state = Self::state_from_apply(&spec, &applied);
                    self.topologies.upsert(state.clone());
                    info!(
                        network_id = %spec.network_id,
                        bridge = %spec.bridge_name,
                        "recovered journaled network topology"
                    );
                    // Policy must re-materialize too (§6.4): nftables survives
                    // a process restart but not a reboot, and the in-memory
                    // record is what re-scopes the guard set on the next NIC
                    // attach. Re-applying here is fail-closed: a policy that
                    // cannot be restored aborts startup rather than serving an
                    // unguarded tenant network.
                    if let Err(e) = self.recover_policies(&spec.network_id, &state).await {
                        error!(
                            network_id = %spec.network_id,
                            error = %e,
                            "failed to recover journaled network policy"
                        );
                        failures.push(format!("{} policy: {e}", spec.network_id));
                    }
                }
                Err(e) => {
                    error!(
                        network_id = %spec.network_id,
                        error = %e,
                        "failed to recover journaled network topology"
                    );
                    failures.push(format!("{}: {e}", spec.network_id));
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(ChvError::Internal {
                reason: format!(
                    "failed to recover {} journaled network topology(ies): {}",
                    failures.len(),
                    failures.join("; ")
                ),
            })
        }
    }

    /// Restore and re-apply the last requested firewall/NAT policy for one
    /// recovered topology. A half pair in the journal is treated as corruption
    /// rather than silently dropped: a firewall payload without its version
    /// (or vice versa) would otherwise leave the network unguarded.
    async fn recover_policies(
        &self,
        network_id: &str,
        state: &TopologyState,
    ) -> Result<(), ChvError> {
        let Some(store) = self.policy_store.as_ref() else {
            return Ok(());
        };
        let Some(policies) = store.load(network_id).await? else {
            return Ok(());
        };
        if policies.is_empty() {
            return Ok(());
        }

        let firewall = match (policies.firewall.clone(), policies.firewall_version.clone()) {
            (Some(json), Some(version)) => Some((json, version)),
            (None, None) => None,
            _ => {
                return Err(ChvError::Internal {
                    reason: format!(
                    "policy journal entry for {network_id} has a firewall payload/version mismatch"
                ),
                })
            }
        };
        let nat = match (policies.nat.clone(), policies.nat_version.clone()) {
            (Some(json), Some(version)) => Some((json, version)),
            (None, None) => None,
            _ => {
                return Err(ChvError::Internal {
                    reason: format!(
                        "policy journal entry for {network_id} has a NAT payload/version mismatch"
                    ),
                })
            }
        };

        {
            let mut applied = self.policy_state.entry(network_id.to_string()).or_default();
            applied.firewall = policies.firewall;
            applied.firewall_version = policies.firewall_version;
            applied.nat = policies.nat;
            applied.nat_version = policies.nat_version;
        }

        if let Some((json, version)) = firewall {
            self.executor
                .set_firewall_policy(network_id, &version, &json, &state.bridge_name)
                .await
                .map_err(|e| {
                    // Name the exact file. A payload-level rejection here
                    // means a persisted entry diverged (corruption,
                    // tampering, or a writer that predates RPC validation);
                    // an environmental nft failure is also possible, so the
                    // message must not imply the entry is always the culprit.
                    // Recovery stays fail-closed: removing the entry without
                    // re-applying leaves the base hooks at `policy accept`.
                    ChvError::Internal {
                        reason: format!(
                            "cannot re-apply journaled firewall policy for {network_id} \
                             from {}: {e}; recovery stays fail-closed. Re-apply the \
                             network policy once nwd is serving, or replace that entry's \
                             payload with [] (default-deny) before restarting chv-nwd; \
                             deleting the entry leaves the network unguarded",
                            store.path_for(network_id).display()
                        ),
                    }
                })?;
        }
        if let Some((json, version)) = nat {
            self.executor
                .set_nat_policy(network_id, &version, &json, &state.bridge_name)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!(
                        "cannot re-apply journaled NAT policy for {network_id} from {}: \
                         {e}; recovery stays fail-closed. Re-apply the network policy \
                         once nwd is serving, or replace that entry's payload with [] \
                         before restarting chv-nwd",
                        store.path_for(network_id).display()
                    ),
                })?;
        }
        info!(
            network_id = %network_id,
            "recovered journaled network policy"
        );
        Ok(())
    }

    fn state_from_apply(
        spec: &proto::TopologySpec,
        applied: &TopologyApplyResult,
    ) -> TopologyState {
        let vni = if spec.vni > 0 { Some(spec.vni) } else { None };
        TopologyState {
            network_id: spec.network_id.clone(),
            tenant_id: spec.tenant_id.clone(),
            bridge_name: spec.bridge_name.clone(),
            namespace_name: spec.namespace_name.clone(),
            subnet_cidr: spec.subnet_cidr.clone(),
            gateway_ip: spec.gateway_ip.clone(),
            runtime_status: "ensured".to_string(),
            vni,
            tenant_mtu: applied.tenant_mtu,
            fabric_plan_generation: applied.fabric_plan_generation,
            binding_generation: applied.binding_generation,
        }
    }

    /// Re-assert the last applied firewall/NAT policy for a topology so its
    /// CHV-owned interface guard set includes any newly enslaved NIC. Fails
    /// closed (returns an error) so an attached NIC is never left outside the
    /// default-deny boundary.
    async fn refresh_policy_scope(&self, state: &TopologyState) -> Result<(), ChvError> {
        // Snapshot semantics: if no record exists for the network there is
        // nothing to re-scope. Re-read the latest desired JSON/version
        // immediately before each apply so a concurrent policy RPC that landed
        // after an earlier snapshot is never reverted by stale desired-state.
        if self.policy_state.get(&state.network_id).is_none() {
            return Ok(());
        }
        let firewall = self
            .policy_state
            .get(&state.network_id)
            .and_then(|p| p.firewall.clone().zip(p.firewall_version.clone()));
        if let Some((json, ver)) = firewall {
            self.executor
                .set_firewall_policy(&state.network_id, &ver, &json, &state.bridge_name)
                .await?;
        }
        let nat = self
            .policy_state
            .get(&state.network_id)
            .and_then(|p| p.nat.clone().zip(p.nat_version.clone()));
        if let Some((json, ver)) = nat {
            self.executor
                .set_nat_policy(&state.network_id, &ver, &json, &state.bridge_name)
                .await?;
        }
        Ok(())
    }

    pub fn topologies(&self) -> Arc<TopologyTable> {
        self.topologies.clone()
    }

    fn ok_result() -> proto::Result {
        let (status, error_code, human_summary) = ChvError::ok_result_fields();
        proto::Result {
            status: status.to_string(),
            error_code: error_code.to_string(),
            human_summary,
        }
    }

    fn err_result(e: &ChvError) -> proto::Result {
        let (status, error_code, human_summary) = e.to_result_fields();
        proto::Result {
            status: status.to_string(),
            error_code: error_code.to_string(),
            human_summary,
        }
    }

    fn map_topology_spec(t: Option<proto::TopologySpec>) -> Result<proto::TopologySpec, ChvError> {
        t.ok_or_else(|| ChvError::InvalidArgument {
            field: "topology".to_string(),
            reason: "missing".to_string(),
        })
    }
}

#[tonic::async_trait]
impl<E: NetworkExecutor> proto::network_service_server::NetworkService for NetworkServiceImpl<E> {
    async fn ensure_network_topology(
        &self,
        request: Request<proto::EnsureNetworkTopologyRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("nwd_ensure_network_topology_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let spec = match Self::map_topology_spec(req.topology) {
            Ok(s) => s,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        // The id becomes a journal file name (and already appears in nwd's
        // runtime file names); reject traversal/injection before any host
        // command runs.
        if let Err(e) = crate::topology_store::validate_network_id(&spec.network_id) {
            return Ok(Response::new(Self::err_result(&e)));
        }

        // IFNAMSIZ limit: Linux interface names must be <= 15 bytes
        if spec.bridge_name.len() > 15 {
            let e = ChvError::InvalidArgument {
                field: "bridge_name".to_string(),
                reason: format!(
                    "exceeds IFNAMSIZ limit (15 chars): '{}' is {} chars",
                    spec.bridge_name,
                    spec.bridge_name.len()
                ),
            };
            return Ok(Response::new(Self::err_result(&e)));
        }

        // VNI range validation: VXLAN VNI is a 24-bit field (max 16777215)
        if spec.vni > 16_777_215 {
            let e = ChvError::InvalidArgument {
                field: "vni".to_string(),
                reason: format!("VNI {} exceeds maximum 16777215", spec.vni),
            };
            return Ok(Response::new(Self::err_result(&e)));
        }

        // Input hardening (fail closed): a nonzero VNI requires a fabric
        // plan. The legacy nolearning VXLAN/FDB datapath was retired by
        // ADR-021, so a nonzero VNI without a plan has no datapath behind
        // it — storing `state.vni = Some(vni)` anyway would record state
        // the executor never realized. No in-tree caller does this (the
        // agent always sends vni = 0 for bridge-only topologies, and the
        // bridge-only re-ensure below requires vni == 0), but a direct
        // gRPC client could.
        if spec.vni > 0 && spec.fabric.is_none() {
            let e = ChvError::InvalidArgument {
                field: "vni".to_string(),
                reason: format!(
                    "VNI {} requires a fabric plan: the legacy nolearning VXLAN/FDB \
                     datapath was retired by ADR-021, so a nonzero VNI has no datapath \
                     without one",
                    spec.vni
                ),
            };
            return Ok(Response::new(Self::err_result(&e)));
        }

        // Idempotency and fabric generation fencing (ADR-021 §4): a fabric
        // plan older than the last applied generation for this network is
        // rejected; an unchanged topology (including fabric generation,
        // VNI, and VNI binding generation) returns OK without re-applying.
        //
        // The VNI and its binding generation participate in the equality
        // (M1): a VNI re-bind bumps `binding_generation` while leaving
        // `desired_generation` (and therefore the fabric plan generation)
        // untouched — without these fields the replay would short-circuit
        // and the datapath would keep the OLD VNI, silently cross-bleeding
        // two networks.
        let existing = self.topologies.get(&spec.network_id);
        if let Some(existing) = existing.as_ref() {
            let new_fabric_generation = spec.fabric.as_ref().map(|f| f.plan_generation);
            if let (Some(applied_gen), Some(new_gen)) =
                (existing.fabric_plan_generation, new_fabric_generation)
            {
                if new_gen < applied_gen {
                    let e = ChvError::StaleGeneration {
                        resource: "network".to_string(),
                        id: spec.network_id.clone(),
                        expected: applied_gen.to_string(),
                        got: new_gen.to_string(),
                    };
                    return Ok(Response::new(Self::err_result(&e)));
                }
            }
            let new_vni = if spec.vni > 0 { Some(spec.vni) } else { None };
            let new_binding_generation = spec.fabric.as_ref().map(|f| f.binding_generation);
            // An explicitly carried tenant MTU that differs from the
            // applied one must not be swallowed by the replay
            // short-circuit (m8): the re-apply path re-asserts the MTU on
            // the bridge, its ports, and dnsmasq. MTU 0 means "plan
            // default", which cannot be compared before the apply.
            let explicit_mtu_changed = spec
                .fabric
                .as_ref()
                .map(|f| f.tenant_mtu > 0 && existing.tenant_mtu != Some(f.tenant_mtu))
                .unwrap_or(false);
            if existing.bridge_name == spec.bridge_name
                && existing.namespace_name == spec.namespace_name
                && existing.subnet_cidr == spec.subnet_cidr
                && existing.gateway_ip == spec.gateway_ip
                && existing.fabric_plan_generation == new_fabric_generation
                && existing.vni == new_vni
                && existing.binding_generation == new_binding_generation
                && !explicit_mtu_changed
            {
                // B9: the table can already hold the topology while the
                // journal is missing (a persist that failed after the apply,
                // or an operator-cleaned journal). Repair it on the
                // idempotent path instead of returning OK with no durable
                // record.
                if let Err(e) = self.repair_topology(&spec).await {
                    return Ok(Response::new(Self::err_result(&e)));
                }
                return Ok(Response::new(Self::ok_result()));
            }

            // Fabric → bridge-only re-ensure (m7): the topology previously
            // had a fabric overlay applied, and the new request carries no
            // fabric plan (bridge-only). Remove the fabric overlay so the
            // applied fabric object does not leak; the local re-ensure
            // below rebuilds the bridge-only topology and the state upsert
            // clears the fabric fields. Teardown semantics are fail-open
            // (warn-and-continue) like the delete path — the residue stays
            // visible via the provider ownership journal.
            if existing.fabric_plan_generation.is_some() && spec.fabric.is_none() && spec.vni == 0 {
                match self.executor.remove_fabric_overlay(&spec.network_id).await {
                    // Known inconsistency (unreachable today): with the
                    // provider disabled, this path counts a remove
                    // "failure" via fabric_handle()'s error, while the
                    // delete path counts nothing (remove not attempted).
                    // The executor's fabric handle is fixed at nwd
                    // construction, so the config cannot toggle under a
                    // running daemon; if that ever changes, make this
                    // path distinguish "not attempted" from "attempted
                    // and failed" like delete_topology does.
                    Ok(()) => {
                        self.metrics.increment_nwd_fabric_remove("success");
                        info!(
                            network_id = %spec.network_id,
                            "fabric overlay removed on bridge-only re-ensure"
                        );
                        // m7 residue cleanup: the removed overlay leaves
                        // the tenant bridge at the fabric MTU and the
                        // running dnsmasq advertising `dhcp-option=26` —
                        // nothing in the plain bridge-only re-ensure below
                        // resets either. Reset both to the bridge-only
                        // defaults (kernel-default MTU, no option 26) via
                        // the m8 re-assert machinery, targeting the
                        // RUNNING topology's bridge/subnet/gateway from
                        // the existing state. Fail closed: the new state
                        // records `tenant_mtu: None`, so the datapath must
                        // actually be at the default before it is
                        // persisted (a failure leaves the old state in
                        // place; the retry re-enters this branch because
                        // fabric removal is idempotent).
                        if let Err(e) = self
                            .executor
                            .reassert_tenant_mtu(
                                &spec.network_id,
                                &existing.bridge_name,
                                &existing.subnet_cidr,
                                &existing.gateway_ip,
                                None,
                            )
                            .await
                        {
                            return Ok(Response::new(Self::err_result(&e)));
                        }
                    }
                    Err(e) => {
                        self.metrics.increment_nwd_fabric_remove("failure");
                        warn!(
                            network_id = %spec.network_id,
                            error = %e,
                            "fabric overlay removal failed on bridge-only re-ensure; \
                             continuing with the local re-ensure (residue remains visible \
                             via the provider ownership journal)"
                        );
                    }
                }
            }
        }

        let fabric_intent = spec.fabric.is_some();
        let result = self.executor.ensure_topology(&spec).await;
        match result {
            Ok(applied) => {
                if applied.fabric_plan_generation.is_some() {
                    self.metrics.increment_nwd_fabric_apply("success");
                }
                // Tenant MTU change (m8): the running dnsmasq still
                // advertises the old DHCP option 26 and the existing bridge
                // ports (TAPs, fabric consumer veth) still carry the old
                // MTU. Re-assert both before persisting the new state.
                // Bounded: only when a previously applied MTU changed.
                if let (Some(old_mtu), Some(new_mtu)) = (
                    existing.as_ref().and_then(|s| s.tenant_mtu),
                    applied.tenant_mtu,
                ) {
                    if old_mtu != new_mtu {
                        if let Err(e) = self
                            .executor
                            .reassert_tenant_mtu(
                                &spec.network_id,
                                &spec.bridge_name,
                                &spec.subnet_cidr,
                                &spec.gateway_ip,
                                Some(new_mtu),
                            )
                            .await
                        {
                            return Ok(Response::new(Self::err_result(&e)));
                        }
                    }
                }
                // Journal BEFORE the in-memory upsert: if the write fails,
                // the retry cannot short-circuit on a topology whose durable
                // record is missing.
                if let Err(e) = self.journal_topology(&spec).await {
                    return Ok(Response::new(Self::err_result(&e)));
                }
                let state = Self::state_from_apply(&spec, &applied);
                self.topologies.upsert(state);
                Ok(Response::new(Self::ok_result()))
            }
            Err(e) => {
                if fabric_intent {
                    self.metrics.increment_nwd_fabric_apply("failure");
                }
                Ok(Response::new(Self::err_result(&e)))
            }
        }
    }

    async fn delete_network_topology(
        &self,
        request: Request<proto::DeleteNetworkTopologyRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("nwd_delete_network_topology_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        if let Some(state) = self.topologies.get(&req.network_id) {
            let fabric_was_applied = state.fabric_plan_generation.is_some();
            match self.executor.delete_topology(&req.network_id, &state).await {
                Ok(outcome) => {
                    // Count the fabric-half outcome truthfully from the
                    // executor's report (n11): the fabric teardown inside
                    // delete_topology is fail-open (m5), so the aggregate
                    // Ok is NOT evidence that the fabric overlay was
                    // removed. The previous approximation proxied this
                    // metric from the aggregate result — counting a fabric
                    // success even when the fail-open removal had failed,
                    // and a fabric failure on a purely local teardown
                    // error.
                    if fabric_was_applied {
                        match outcome.fabric_removed {
                            Some(Ok(())) => {
                                self.metrics.increment_nwd_fabric_remove("success");
                            }
                            Some(Err(ref e)) => {
                                self.metrics.increment_nwd_fabric_remove("failure");
                                warn!(
                                    network_id = %req.network_id,
                                    error = %e,
                                    "fabric overlay removal failed during topology delete; \
                                     local teardown completed (fail-open for teardown only — \
                                     apply stays fail-closed; residue remains visible via the \
                                     provider ownership journal)"
                                );
                            }
                            None => {
                                // Fabric teardown was not attempted (the
                                // provider is disabled in configuration);
                                // the executor already warned. There was
                                // no removal, so there is no outcome to
                                // count.
                            }
                        }
                    }
                    self.topologies.remove(&req.network_id);
                }
                Err(e) => {
                    // Local teardown failure. The fabric half ran first
                    // inside the executor, but its outcome is not
                    // observable through this error, so the fabric-remove
                    // metric is deliberately NOT proxied from the
                    // aggregate outcome (the removed approximation counted
                    // this as a fabric failure even when the fabric half
                    // had succeeded).
                    return Ok(Response::new(Self::err_result(&e)));
                }
            }
        } else {
            // No local topology state row (typically after an nwd restart
            // wiped the in-memory table), but the fabric provider's durable
            // ownership journal may still hold the network (M3). A delete
            // must not silently no-op while the fabric network survives
            // forever — bounded fix: check ownership and tear the fabric
            // half down. (Full startup reconciliation stays a Phase-4
            // item.) A fabric teardown failure must not turn this delete
            // into an error — there is no local topology to fail on — but
            // it is loudly visible.
            match self.executor.fabric_owned(&req.network_id).await {
                Ok(FabricOwnership::Owned) => {
                    match self.executor.remove_fabric_overlay(&req.network_id).await {
                        Ok(()) => {
                            self.metrics.increment_nwd_fabric_remove("success");
                            info!(
                                network_id = %req.network_id,
                                "fabric overlay removed for network with no local topology \
                                 state (provider ownership journal held it across a restart)"
                            );
                        }
                        Err(e) => {
                            self.metrics.increment_nwd_fabric_remove("failure");
                            warn!(
                                network_id = %req.network_id,
                                error = %e,
                                "fabric overlay removal failed for network with no local \
                                 topology state; the fabric network may outlive the delete \
                                 (residue remains visible via the provider ownership journal)"
                            );
                        }
                    }
                }
                Ok(FabricOwnership::NotOwned) => {
                    // Provider enabled and holds no entry: nothing to do —
                    // the current no-state behavior.
                }
                Ok(FabricOwnership::ProviderDisabled) => {
                    // Residue case (M3 observability gap): the fabric
                    // overlay may have been applied before an nwd restart
                    // that came up with the fabric provider disabled in
                    // configuration — ownership is unobservable in this
                    // process and the delete cannot tear the fabric half
                    // down. Loud, not silent; the RPC result stays Ok
                    // (nothing local failed).
                    warn!(
                        network_id = %req.network_id,
                        "delete for a network with no local topology state found the \
                         fabric provider disabled in nwd configuration; any fabric residue \
                         cannot be observed or torn down until the provider is re-enabled \
                         (residue remains visible via the provider ownership journal)"
                    );
                }
                // Ownership cannot be determined: fail closed rather than
                // silently no-op (the original leak was exactly a silent
                // OK).
                Err(e) => {
                    return Ok(Response::new(Self::err_result(&e)));
                }
            }
            // #356 N5: with no state row, the local half (bridge /
            // namespace / dnsmasq / nft table) used to be silently
            // SKIPPED — the restart residue the M4.4 qualification
            // observed. Every local name is derived deterministically
            // from the network id, so run the local teardown by derived
            // names (existence-gated, warn-only), mirroring the fabric
            // half's ownership-journal fallback above. Gated on fabric
            // ownership being determinable: an ownership error above
            // returns first (fail closed, residue stays observable).
            self.executor
                .delete_local_topology_by_derived_names(&req.network_id)
                .await;
        }
        // B9: remove the journal entry once the host topology is gone (or was
        // already gone). A failure here leaves a durable record that would
        // resurrect the topology on the next restart, so the delete fails
        // closed and the idempotent retry clears it.
        if let Err(e) = self.forget_topology(&req.network_id).await {
            return Ok(Response::new(Self::err_result(&e)));
        }
        // Drop the durable policy for the same reason: a stale policy
        // re-applied after a restart against a re-created network with the
        // same id would be the #227 S5 residue class. This also drops the
        // in-memory record, which is only meaningful with a topology. Take
        // the persistence lock so a policy RPC that already passed its
        // topology check cannot re-persist an entry after this removal.
        {
            let _guard = self.policy_persist.lock().await;
            if let Err(e) = self.forget_policies(&req.network_id).await {
                return Ok(Response::new(Self::err_result(&e)));
            }
            self.policy_state.remove(&req.network_id);
        }

        Ok(Response::new(Self::ok_result()))
    }

    async fn get_network_health(
        &self,
        request: Request<proto::NetworkHealthRequest>,
    ) -> Result<Response<proto::NetworkHealthResponse>, Status> {
        let req = request.into_inner();

        let (status, last_error) = if let Some(state) = self.topologies.get(&req.network_id) {
            match self.executor.health(&req.network_id, &state).await {
                Ok(s) => {
                    // Downgrade health if eBPF has load failures
                    let ebpf_failures = self.ebpf_load_failures.load(Ordering::Relaxed);
                    if ebpf_failures > 0 && s == "healthy" {
                        (
                            "degraded".to_string(),
                            format!(
                                "eBPF load failures: {}; traffic may pass unfiltered",
                                ebpf_failures
                            ),
                        )
                    } else {
                        (s, String::new())
                    }
                }
                Err(e) => ("unhealthy".to_string(), e.to_string()),
            }
        } else {
            ("unknown".to_string(), String::new())
        };

        Ok(Response::new(proto::NetworkHealthResponse {
            result: Some(Self::ok_result()),
            network_id: req.network_id,
            health_status: status,
            last_error,
        }))
    }

    async fn list_namespace_state(
        &self,
        _request: Request<proto::ListNamespaceStateRequest>,
    ) -> Result<Response<proto::ListNamespaceStateResponse>, Status> {
        let items: Vec<proto::NamespaceState> = self
            .topologies
            .list()
            .into_iter()
            .map(|s| proto::NamespaceState {
                network_id: s.network_id,
                namespace_name: s.namespace_name,
                bridge_name: s.bridge_name,
                runtime_status: s.runtime_status,
            })
            .collect();

        Ok(Response::new(proto::ListNamespaceStateResponse { items }))
    }

    async fn attach_vm_nic(
        &self,
        request: Request<proto::AttachVmNicRequest>,
    ) -> Result<Response<proto::AttachVmNicResponse>, Status> {
        self.metrics.increment_counter("nwd_attach_vm_nic_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let nic = req.nic.ok_or_else(|| ChvError::InvalidArgument {
            field: "nic".to_string(),
            reason: "missing".to_string(),
        });
        let nic = match nic {
            Ok(n) => n,
            Err(e) => {
                return Ok(Response::new(proto::AttachVmNicResponse {
                    result: Some(Self::err_result(&e)),
                    namespace_handle: String::new(),
                    tap_handle: String::new(),
                }));
            }
        };

        let state = match self.topologies.get(&nic.network_id) {
            Some(s) => s,
            None => {
                let e = ChvError::NotFound {
                    resource: "topology".to_string(),
                    id: nic.network_id.clone(),
                };
                return Ok(Response::new(proto::AttachVmNicResponse {
                    result: Some(Self::err_result(&e)),
                    namespace_handle: String::new(),
                    tap_handle: String::new(),
                }));
            }
        };

        match self
            .executor
            .attach_vm_nic(
                &nic.network_id,
                &nic.nic_id,
                &nic.vm_id,
                &state.bridge_name,
                state.tenant_mtu,
                &nic.mac_address,
                &nic.ip_address,
            )
            .await
        {
            Ok((namespace_handle, tap_handle)) => {
                // Auto-load eBPF programs on the new tap interface — MANDATORY for tenant isolation
                let bridge_name = state.bridge_name.clone();
                if let Err(e) = self.ebpf.load_policy_program(&tap_handle).await {
                    tracing::error!(tap = %tap_handle, error = %e, "eBPF policy load failed — refusing NIC attach (default-deny)");
                    self.ebpf_load_failures.fetch_add(1, Ordering::Relaxed);
                    // Detach the NIC we just attached to avoid dangling resources
                    let _ = self
                        .executor
                        .detach_vm_nic(
                            &nic.nic_id,
                            chv_common::AttachmentOwnership {
                                vm_id: nic.vm_id.clone(),
                                operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                                requester: None,
                            },
                        )
                        .await;
                    let err = ChvError::Internal {
                        reason: format!(
                            "eBPF policy program failed to load on tap {tap_handle}: {e}"
                        ),
                    };
                    return Ok(Response::new(proto::AttachVmNicResponse {
                        result: Some(Self::err_result(&err)),
                        namespace_handle: String::new(),
                        tap_handle: String::new(),
                    }));
                }
                if let Err(e) = self.ebpf.load_ingress_program(&bridge_name).await {
                    tracing::error!(bridge = %bridge_name, error = %e, "eBPF ingress load failed — refusing NIC attach (default-deny)");
                    self.ebpf_load_failures.fetch_add(1, Ordering::Relaxed);
                    // Detach the NIC we just attached to avoid dangling resources
                    let _ = self
                        .executor
                        .detach_vm_nic(
                            &nic.nic_id,
                            chv_common::AttachmentOwnership {
                                vm_id: nic.vm_id.clone(),
                                operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                                requester: None,
                            },
                        )
                        .await;
                    let err = ChvError::Internal {
                        reason: format!(
                            "eBPF ingress program failed to load on bridge {bridge_name}: {e}"
                        ),
                    };
                    return Ok(Response::new(proto::AttachVmNicResponse {
                        result: Some(Self::err_result(&err)),
                        namespace_handle: String::new(),
                        tap_handle: String::new(),
                    }));
                }

                // Re-scope the CHV firewall/NAT guard sets so the newly attached
                // NIC is covered by CHV default-deny. Fail closed: if a previously
                // applied policy cannot be refreshed, refuse the attach rather
                // than leave the new interface undispatched and unprotected.
                if let Err(e) = self.refresh_policy_scope(&state).await {
                    tracing::error!(
                        network_id = %nic.network_id,
                        error = %e,
                        "policy guard refresh failed — refusing NIC attach (default-deny)"
                    );
                    let _ = self
                        .executor
                        .detach_vm_nic(
                            &nic.nic_id,
                            chv_common::AttachmentOwnership {
                                vm_id: nic.vm_id.clone(),
                                operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                                requester: None,
                            },
                        )
                        .await;
                    let err = ChvError::Internal {
                        reason: format!(
                            "CHV policy guard refresh failed on attach (tap {tap_handle}): {e}"
                        ),
                    };
                    return Ok(Response::new(proto::AttachVmNicResponse {
                        result: Some(Self::err_result(&err)),
                        namespace_handle: String::new(),
                        tap_handle: String::new(),
                    }));
                }

                Ok(Response::new(proto::AttachVmNicResponse {
                    result: Some(Self::ok_result()),
                    namespace_handle,
                    tap_handle,
                }))
            }
            Err(e) => Ok(Response::new(proto::AttachVmNicResponse {
                result: Some(Self::err_result(&e)),
                namespace_handle: String::new(),
                tap_handle: String::new(),
            })),
        }
    }

    async fn detach_vm_nic(
        &self,
        request: Request<proto::DetachVmNicRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("nwd_detach_vm_nic_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        match self
            .executor
            .detach_vm_nic(
                &req.nic_id,
                chv_common::AttachmentOwnership {
                    vm_id: req.vm_id.clone(),
                    operation_id: req.meta.as_ref().map(|m| m.operation_id.clone()),
                    requester: None,
                },
            )
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn set_firewall_policy(
        &self,
        request: Request<proto::SetFirewallPolicyRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("nwd_set_firewall_policy_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let policy = req.policy.ok_or_else(|| ChvError::InvalidArgument {
            field: "policy".to_string(),
            reason: "missing".to_string(),
        });
        let policy = match policy {
            Ok(p) => p,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        // CHV firewall policy must be scoped to a CHV-owned interface. Without a
        // known topology owner, fail closed rather than guess a host interface.
        let state = match self.topologies.get(&req.network_id) {
            Some(s) => s,
            None => {
                let e = ChvError::NotFound {
                    resource: "topology".to_string(),
                    id: req.network_id.clone(),
                };
                return Ok(Response::new(Self::err_result(&e)));
            }
        };

        // Reject a payload the engine can never apply BEFORE journaling it as
        // desired state. The journal is replayed fail-closed before serving,
        // so an unapplicable entry would turn the next documented restart
        // into a crash loop that only manual file surgery could clear.
        if let Err(e) = crate::firewall::validate_firewall_policy(&policy.policy_json) {
            return Ok(Response::new(Self::err_result(&e)));
        }

        // Record the DESIRED policy state regardless of apply outcome (the
        // topology is ensured at this point). A failed or partial apply must not
        // leave policy_state empty, otherwise a later NIC attach would find
        // nothing to re-scope and the new member could sit outside the CHV
        // boundary (fail-open, #227 S3). refresh_policy_scope re-applies the
        // desired policy, so the boundary converges on the next attach.
        //
        // The same record is written to the durable policy journal, so an nwd
        // restart (or host reboot) restores the guard set instead of serving a
        // recovered topology with no policy. The in-memory pair and the
        // persisted snapshot are mutated under one lock, so a concurrent NAT
        // RPC cannot lose the firewall half (and vice versa).
        if let Err(e) = self
            .record_firewall_policy(&req.network_id, &policy.policy_json, &policy.policy_version)
            .await
        {
            return Ok(Response::new(Self::err_result(&e)));
        }

        match self
            .executor
            .set_firewall_policy(
                &req.network_id,
                &policy.policy_version,
                &policy.policy_json,
                &state.bridge_name,
            )
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn set_nat_policy(
        &self,
        request: Request<proto::SetNatPolicyRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("nwd_set_nat_policy_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let policy = req.policy.ok_or_else(|| ChvError::InvalidArgument {
            field: "policy".to_string(),
            reason: "missing".to_string(),
        });
        let policy = match policy {
            Ok(p) => p,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        // Same fail-closed ownership requirement as set_firewall_policy.
        let state = match self.topologies.get(&req.network_id) {
            Some(s) => s,
            None => {
                let e = ChvError::NotFound {
                    resource: "topology".to_string(),
                    id: req.network_id.clone(),
                };
                return Ok(Response::new(Self::err_result(&e)));
            }
        };

        // Reject an unapplicable NAT payload before journaling it (see
        // set_firewall_policy): the journal is replayed fail-closed at
        // startup, so a permanently unapplicable entry would crash-loop nwd.
        if let Err(e) = crate::firewall::validate_nat_policy(&policy.policy_json) {
            return Ok(Response::new(Self::err_result(&e)));
        }

        // Record the DESIRED NAT policy state regardless of apply outcome for
        // the same reason as set_firewall_policy (re-scope convergence, #227
        // S3), and persist it to the durable policy journal so restart/reboot
        // recovery restores the NAT guard as well. The in-memory pair and the
        // persisted snapshot are mutated under one lock (see
        // set_firewall_policy) so a concurrent firewall RPC cannot lose either
        // half of the pair.
        if let Err(e) = self
            .record_nat_policy(&req.network_id, &policy.policy_json, &policy.policy_version)
            .await
        {
            return Ok(Response::new(Self::err_result(&e)));
        }

        match self
            .executor
            .set_nat_policy(
                &req.network_id,
                &policy.policy_version,
                &policy.policy_json,
                &state.bridge_name,
            )
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn ensure_dhcp_scope(
        &self,
        request: Request<proto::EnsureDhcpScopeRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("nwd_ensure_dhcp_scope_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let scope = req.scope.ok_or_else(|| ChvError::InvalidArgument {
            field: "scope".to_string(),
            reason: "missing".to_string(),
        });
        let scope = match scope {
            Ok(s) => s,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        match self
            .executor
            .ensure_dhcp_scope(
                &scope.network_id,
                &scope.cidr,
                &scope.range_start,
                &scope.range_end,
                &scope.dns_servers,
            )
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn ensure_dns_scope(
        &self,
        request: Request<proto::EnsureDnsScopeRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("nwd_ensure_dns_scope_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let scope = req.scope.ok_or_else(|| ChvError::InvalidArgument {
            field: "scope".to_string(),
            reason: "missing".to_string(),
        });
        let scope = match scope {
            Ok(s) => s,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        let fw: Vec<&str> = scope.forwarders.iter().map(|s| s.as_str()).collect();
        let static_records: std::collections::HashMap<String, String> =
            scope.static_records.into_iter().collect();
        match self
            .executor
            .ensure_dns_scope(&scope.network_id, &fw, &static_records)
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn expose_service(
        &self,
        request: Request<proto::ExposeServiceRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics.increment_counter("nwd_expose_service_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        let exposure = req.exposure.ok_or_else(|| ChvError::InvalidArgument {
            field: "exposure".to_string(),
            reason: "missing".to_string(),
        });
        let exposure = match exposure {
            Ok(e) => e,
            Err(e) => return Ok(Response::new(Self::err_result(&e))),
        };

        match self
            .executor
            .expose_service(
                &exposure.network_id,
                &exposure.exposure_id,
                &exposure.protocol,
                exposure.external_port,
                &exposure.target_ip,
                exposure.target_port,
                &exposure.mode,
            )
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn withdraw_service_exposure(
        &self,
        request: Request<proto::WithdrawServiceExposureRequest>,
    ) -> Result<Response<proto::Result>, Status> {
        self.metrics
            .increment_counter("nwd_withdraw_service_exposure_total");
        let req = request.into_inner();
        let _span = req
            .meta
            .as_ref()
            .map(|m| operation_span(&m.operation_id))
            .unwrap_or_else(|| operation_span(""));

        match self
            .executor
            .withdraw_service_exposure(&req.network_id, &req.exposure_id)
            .await
        {
            Ok(()) => Ok(Response::new(Self::ok_result())),
            Err(e) => Ok(Response::new(Self::err_result(&e))),
        }
    }

    async fn update_overlay(
        &self,
        request: Request<proto::UpdateOverlayRequest>,
    ) -> Result<Response<proto::UpdateOverlayResponse>, Status> {
        self.metrics.increment_counter("nwd_update_overlay_total");
        let req = request.into_inner();

        let state = match self.topologies.get(&req.network_id) {
            Some(s) => s,
            None => {
                let e = ChvError::NotFound {
                    resource: "topology".to_string(),
                    id: req.network_id.clone(),
                };
                return Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        };

        if req.vni == 0 {
            let e = ChvError::InvalidArgument {
                field: "vni".to_string(),
                reason: "VNI must be > 0 for overlay update".to_string(),
            };
            return Ok(Response::new(proto::UpdateOverlayResponse {
                result: Some(Self::err_result(&e)),
            }));
        }

        // VNI range validation: VXLAN VNI is a 24-bit field (max 16777215)
        if req.vni > 16_777_215 {
            let e = ChvError::InvalidArgument {
                field: "vni".to_string(),
                reason: format!("VNI {} exceeds maximum 16777215", req.vni),
            };
            return Ok(Response::new(proto::UpdateOverlayResponse {
                result: Some(Self::err_result(&e)),
            }));
        }

        // Stretched-L2 fabric path (ADR-021): the legacy nolearning
        // VXLAN/FDB datapath was retired; a fabric plan is required.
        // Generation fencing happens here (the handler owns the topology
        // table); the executor applies, grafts the consumer veth into the
        // tenant bridge, and returns the applied generation/MTU for state
        // persistence.
        let fabric_plan = match req.fabric.as_ref() {
            Some(plan) => plan,
            None => {
                let e = ChvError::InvalidArgument {
                    field: "fabric".to_string(),
                    reason: "the legacy nolearning VXLAN/FDB datapath was retired by ADR-021; \
                            a fabric plan is required for overlay updates"
                        .to_string(),
                };
                return Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        };

        if let Some(applied_gen) = state.fabric_plan_generation {
            if fabric_plan.plan_generation < applied_gen {
                let e = ChvError::StaleGeneration {
                    resource: "network".to_string(),
                    id: req.network_id.clone(),
                    expected: applied_gen.to_string(),
                    got: fabric_plan.plan_generation.to_string(),
                };
                return Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        }

        // VNI binding fence (M1): a binding_generation LOWER than the last
        // applied one is a stale binding (e.g. a VNI re-bind raced by an
        // older in-flight plan); applying it would strand the datapath on
        // an outdated VNI. A higher or changed binding generation proceeds
        // to re-apply — the update path never short-circuits.
        if let Some(applied_binding) = state.binding_generation {
            if fabric_plan.binding_generation < applied_binding {
                let e = ChvError::StaleGeneration {
                    resource: "network vni binding".to_string(),
                    id: req.network_id.clone(),
                    expected: applied_binding.to_string(),
                    got: fabric_plan.binding_generation.to_string(),
                };
                return Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        }

        return match self
            .executor
            .apply_fabric_overlay(&req.network_id, req.vni, fabric_plan, &state.bridge_name)
            .await
        {
            Ok(applied) => {
                self.metrics.increment_nwd_fabric_apply("success");
                let updated_state = TopologyState {
                    vni: Some(req.vni),
                    tenant_mtu: Some(applied.tenant_mtu),
                    fabric_plan_generation: Some(applied.plan_generation),
                    binding_generation: Some(applied.binding_generation),
                    ..state.clone()
                };
                self.topologies.upsert(updated_state);
                // Re-assert the CHV firewall/NAT guard scope: the fabric
                // consumer veth just joined the tenant bridge (same
                // reasoning as NIC attach; fail closed).
                if let Err(e) = self.refresh_policy_scope(&state).await {
                    return Ok(Response::new(proto::UpdateOverlayResponse {
                        result: Some(Self::err_result(&e)),
                    }));
                }
                info!(
                    network_id = %req.network_id,
                    vni = req.vni,
                    plan_generation = applied.plan_generation,
                    tenant_mtu = applied.tenant_mtu,
                    "fabric overlay updated"
                );
                Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::ok_result()),
                }))
            }
            Err(e) => {
                self.metrics.increment_nwd_fabric_apply("failure");
                Ok(Response::new(proto::UpdateOverlayResponse {
                    result: Some(Self::err_result(&e)),
                }))
            }
        };
    }

    async fn send_gratuitous_arp(
        &self,
        request: Request<proto::SendGratuitousArpRequest>,
    ) -> Result<Response<proto::SendGratuitousArpResponse>, Status> {
        self.metrics
            .increment_counter("nwd_send_gratuitous_arp_total");
        let req = request.into_inner();

        let state = match self.topologies.get(&req.network_id) {
            Some(s) => s,
            None => {
                let e = ChvError::NotFound {
                    resource: "topology".to_string(),
                    id: req.network_id.clone(),
                };
                return Ok(Response::new(proto::SendGratuitousArpResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        };

        if let Err(e) = self
            .executor
            .send_gratuitous_arp(&state.namespace_name, &req.bridge_name, &req.vm_ip)
            .await
        {
            return Ok(Response::new(proto::SendGratuitousArpResponse {
                result: Some(Self::err_result(&e)),
            }));
        }

        info!(
            network_id = %req.network_id,
            vm_ip = %req.vm_ip,
            bridge_name = %req.bridge_name,
            "gratuitous ARP sent"
        );

        Ok(Response::new(proto::SendGratuitousArpResponse {
            result: Some(Self::ok_result()),
        }))
    }

    async fn update_security_policy(
        &self,
        request: Request<proto::SecurityPolicy>,
    ) -> Result<Response<proto::UpdateSecurityPolicyResponse>, Status> {
        self.metrics
            .increment_counter("nwd_update_security_policy_total");
        let policy = request.into_inner();

        let key = format!("{}:{}", policy.network_id, policy.vm_id);
        let vm_id = policy.vm_id.clone();
        let default_action = if policy.default_action == proto::PolicyAction::PolicyAllow as i32 {
            1u8
        } else {
            0u8
        };

        // Convert proto rules to eBPF rules; an overlong vm_id is rejected
        // rather than silently truncated into a colliding map key.
        let ebpf_rules = match ebpf::proto_to_ebpf_rules(&vm_id, &policy) {
            Ok(rules) => rules,
            Err(e) => {
                return Ok(Response::new(proto::UpdateSecurityPolicyResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        };

        info!(
            vm_id = %policy.vm_id,
            network_id = %policy.network_id,
            rule_count = policy.rules.len(),
            ebpf_available = self.ebpf.is_available(),
            "security policy stored"
        );
        self.security_policies.insert(key, policy);

        // Push rules to eBPF maps
        if let Err(e) = self.ebpf.update_rules(&vm_id, &ebpf_rules).await {
            tracing::warn!(vm_id = %vm_id, error = %e, "failed to update eBPF rules");
        }
        if let Err(e) = self.ebpf.set_default_action(&vm_id, default_action).await {
            tracing::warn!(vm_id = %vm_id, error = %e, "failed to set eBPF default action");
        }

        Ok(Response::new(proto::UpdateSecurityPolicyResponse {
            result: Some(Self::ok_result()),
        }))
    }

    async fn update_rate_limit(
        &self,
        request: Request<proto::RateLimitPolicy>,
    ) -> Result<Response<proto::UpdateRateLimitResponse>, Status> {
        self.metrics
            .increment_counter("nwd_update_rate_limit_total");
        let policy = request.into_inner();

        let vm_id = policy.vm_id.clone();
        let ebpf_rl = match ebpf::proto_to_ebpf_rate_limit(&policy) {
            Ok(rl) => rl,
            Err(e) => {
                return Ok(Response::new(proto::UpdateRateLimitResponse {
                    result: Some(Self::err_result(&e)),
                }));
            }
        };

        info!(
            vm_id = %policy.vm_id,
            rate_bps = policy.rate_bps,
            burst_bytes = policy.burst_bytes,
            ebpf_available = self.ebpf.is_available(),
            "rate limit policy stored"
        );
        self.rate_limit_policies
            .insert(policy.vm_id.clone(), policy);

        // Push rate limit to eBPF maps
        if let Err(e) = self.ebpf.update_rate_limit(&vm_id, &ebpf_rl).await {
            tracing::warn!(vm_id = %vm_id, error = %e, "failed to update eBPF rate limit");
        }

        Ok(Response::new(proto::UpdateRateLimitResponse {
            result: Some(Self::ok_result()),
        }))
    }

    async fn get_overlay_status(
        &self,
        request: Request<proto::GetOverlayStatusRequest>,
    ) -> Result<Response<proto::OverlayStatus>, Status> {
        let req = request.into_inner();

        let state = match self.topologies.get(&req.network_id) {
            Some(s) => s,
            None => {
                return Ok(Response::new(proto::OverlayStatus {
                    network_id: req.network_id,
                    vni: 0,
                    vxlan_interface_up: false,
                    fdb_entry_count: 0,
                    ebpf_programs_loaded: 0,
                }));
            }
        };

        // Try to determine VNI from topology; for now look it up from state
        // In a full implementation, TopologyState would track VNI.
        // We use a best-effort approach: check if any overlay exists.
        let vni = state.vni.unwrap_or(0);
        if vni == 0 {
            return Ok(Response::new(proto::OverlayStatus {
                network_id: req.network_id,
                vni: 0,
                vxlan_interface_up: false,
                fdb_entry_count: 0,
                ebpf_programs_loaded: 0,
            }));
        }

        // Fabric-backed topologies (ADR-021) report from the fabric
        // provider. The legacy nolearning VXLAN datapath is retired, so a
        // topology without an applied fabric plan reports down.
        let status_info = if state.fabric_plan_generation.is_some() {
            match self.executor.fabric_overlay_status(&req.network_id).await {
                Ok(info) => info,
                // Provider disabled in configuration after the overlay was
                // applied: the overlay state is unobservable in this
                // process; report a truthful "down" rather than failing
                // the read (nothing is invented — the fields stay zero).
                Err(ChvError::InvalidArgument { field, reason })
                    if field == "fabric" && reason.contains("disabled") =>
                {
                    OverlayStatusInfo {
                        vxlan_interface_up: false,
                        fdb_entry_count: 0,
                    }
                }
                // Provider errors (foreign state, IO, ...) are surfaced
                // through the structured ChvError mapping — never masked
                // as a "down" status (m9). OverlayStatus carries no
                // in-band result field, so the mapped tonic Status is the
                // error channel for this RPC.
                Err(e) => return Err(Status::from(e)),
            }
        } else {
            OverlayStatusInfo {
                vxlan_interface_up: false,
                fdb_entry_count: 0,
            }
        };

        Ok(Response::new(proto::OverlayStatus {
            network_id: req.network_id,
            vni,
            vxlan_interface_up: status_info.vxlan_interface_up,
            fdb_entry_count: status_info.fdb_entry_count,
            ebpf_programs_loaded: self.ebpf.loaded_program_count(),
        }))
    }

    async fn get_fabric_identity(
        &self,
        request: Request<proto::GetFabricIdentityRequest>,
    ) -> Result<Response<proto::FabricIdentityResponse>, Status> {
        self.metrics
            .increment_counter("nwd_get_fabric_identity_total");
        let _ = request.into_inner();

        // Fail closed with an in-band error when the fabric provider is
        // disabled: no identity is invented, and no key is ever returned as
        // private material (only the public key leaves this handler).
        match self.executor.fabric_identity().await {
            Ok(identity) => Ok(Response::new(proto::FabricIdentityResponse {
                result: Some(Self::ok_result()),
                public_key: identity.public_key,
                underlay_mtu: identity.underlay_mtu,
            })),
            Err(e) => Ok(Response::new(proto::FabricIdentityResponse {
                result: Some(Self::err_result(&e)),
                public_key: String::new(),
                underlay_mtu: 0,
            })),
        }
    }
}
