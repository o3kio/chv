//! VTEP registry and VNI allocation for overlay networking.

use crate::audit_outbox::{enqueue_on_tx, AuditOutboxInsert};
use crate::{StoreError, StorePool};

/// Repository for VTEP (Virtual Tunnel Endpoint) registry operations.
#[derive(Clone)]
pub struct VtepRepository {
    pool: StorePool,
}

/// A VTEP registry entry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VtepEntry {
    pub node_id: String,
    pub vtep_ip: String,
    pub vtep_port: i32,
    pub updated_at: String,
    /// WireGuard public key (base64) reported by the node (ADR-021).
    pub public_key: Option<String>,
    /// `host:port` of the node's WireGuard listener; NULL until node
    /// underlay addressing is wired up (planner fails closed on NULL).
    pub underlay_endpoint: Option<String>,
    /// Fabric transport IP allocated from 100.100.0.0/16 (ADR-021 §6).
    pub fabric_ip: Option<String>,
    /// Measured underlay MTU; NULL when the node has not measured it.
    pub underlay_mtu: Option<i64>,
}

/// A node's fabric identity, as seen by the plan compiler. Every field is
/// optional at the store level so a partially-registered peer is visible
/// (and rejectable) instead of silently dropped from the flood list.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FabricPeerRecord {
    pub node_id: String,
    pub public_key: Option<String>,
    pub underlay_endpoint: Option<String>,
    pub fabric_ip: Option<String>,
    pub underlay_mtu: Option<i64>,
}

/// Fabric transport addresses are allocated from 100.100.0.0/16 (ADR-021
/// §6). Host range: 100.100.0.1 ..= 100.100.255.254.
const FABRIC_IP_NETWORK_BASE: u32 = (100 << 24) | (100 << 16);
const FABRIC_IP_FIRST: u32 = FABRIC_IP_NETWORK_BASE + 1;
const FABRIC_IP_LAST: u32 = FABRIC_IP_NETWORK_BASE + 65_534;

/// Bounded retry budget when two concurrent registrations race for the
/// same fabric transport IP and one loses on the unique index
/// (migration 0054). Each attempt re-reads the used-address set, so a
/// loser's next attempt skips the address the winner committed.
const FABRIC_IP_ALLOCATION_ATTEMPTS: usize = 5;

/// True when a store error is a unique-index violation on
/// `vtep_registry.fabric_ip` (migration 0054). SQLite surfaces unique
/// violations as database errors whose message names the constraint.
fn is_fabric_ip_unique_violation(err: &StoreError) -> bool {
    match err {
        StoreError::Database(sqlx::Error::Database(db)) => {
            let msg = db.message();
            msg.contains("UNIQUE constraint failed") && msg.contains("vtep_registry.fabric_ip")
        }
        _ => false,
    }
}

impl VtepRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Register or update a VTEP entry for a node.
    pub async fn register_vtep(
        &self,
        node_id: &str,
        vtep_ip: &str,
        vtep_port: i32,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"INSERT INTO vtep_registry (node_id, vtep_ip, vtep_port, updated_at)
               VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
               ON CONFLICT(node_id) DO UPDATE SET
                   vtep_ip = excluded.vtep_ip,
                   vtep_port = excluded.vtep_port,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"#,
        )
        .bind(node_id)
        .bind(vtep_ip)
        .bind(vtep_port)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a single VTEP entry for a node.
    pub async fn get_vtep(&self, node_id: &str) -> Result<VtepEntry, StoreError> {
        sqlx::query_as::<_, VtepEntry>(
            r#"SELECT node_id, vtep_ip, vtep_port, updated_at,
                      public_key, underlay_endpoint, fabric_ip, underlay_mtu
               FROM vtep_registry WHERE node_id = ?"#,
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound {
            entity: "vtep_registry",
            id: node_id.to_string(),
        })
    }

    /// Get a node's fabric identity (public key, endpoint, transport IP,
    /// underlay MTU). `Ok(None)` when the node has no registry row at all.
    pub async fn get_fabric_identity(
        &self,
        node_id: &str,
    ) -> Result<Option<FabricPeerRecord>, StoreError> {
        let record = sqlx::query_as::<_, FabricPeerRecord>(
            r#"SELECT node_id, public_key, underlay_endpoint, fabric_ip, underlay_mtu
               FROM vtep_registry WHERE node_id = ?"#,
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record)
    }

    /// Get VTEPs for all nodes that have VMs on a given network.
    /// This joins vtep_registry with vm placements that are attached to the network.
    pub async fn get_vteps_for_network(
        &self,
        network_id: &str,
    ) -> Result<Vec<VtepEntry>, StoreError> {
        let entries = sqlx::query_as::<_, VtepEntry>(
            r#"SELECT DISTINCT vr.node_id, vr.vtep_ip, vr.vtep_port, vr.updated_at,
                      vr.public_key, vr.underlay_endpoint, vr.fabric_ip, vr.underlay_mtu
               FROM vtep_registry vr
               INNER JOIN vm_desired_state vds ON vds.target_node_id = vr.node_id
               INNER JOIN vm_nic_desired_state vnds ON vnds.vm_id = vds.vm_id
               WHERE vnds.network_id = ?"#,
        )
        .bind(network_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(entries)
    }

    /// Get the fabric identities of every node hosting a VM NIC on the
    /// given network — the bounded flood list of ADR-021 §10. Rows with a
    /// missing public key / endpoint / fabric IP are returned as-is (with
    /// `None` fields) so the plan compiler can fail closed naming the node
    /// instead of silently shrinking the flood list.
    ///
    /// The result is ordered by `node_id` (the store's house ordering for
    /// node lists): the fabric-plan compile order — and therefore the
    /// per-node roll-up order of the all-refusals `Unimplemented` error
    /// from the overlay fan-out — is deterministic only if this query is.
    pub async fn get_fabric_peers_for_network(
        &self,
        network_id: &str,
    ) -> Result<Vec<FabricPeerRecord>, StoreError> {
        let peers = sqlx::query_as::<_, FabricPeerRecord>(
            r#"SELECT DISTINCT vr.node_id, vr.public_key, vr.underlay_endpoint,
                      vr.fabric_ip, vr.underlay_mtu
               FROM vtep_registry vr
               INNER JOIN vm_desired_state vds ON vds.target_node_id = vr.node_id
               INNER JOIN vm_nic_desired_state vnds ON vnds.vm_id = vds.vm_id
               WHERE vnds.network_id = ?
               ORDER BY vr.node_id"#,
        )
        .bind(network_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(peers)
    }

    /// Register or update a node's fabric identity (ADR-021 §5): upserts
    /// the WireGuard public key and measured underlay MTU into
    /// `vtep_registry` and, on first registration, allocates the node's
    /// fabric transport IP from 100.100.0.0/16.
    ///
    /// `underlay_mtu == 0` (the proto default for "not measured") is stored
    /// as NULL.
    ///
    /// `underlay_endpoint` follows FIRST-REGISTRATION-WINS: the stored
    /// value is only written when the node does not have one yet (insert,
    /// or previously NULL). The control-plane callers derive it from the
    /// observed gRPC peer address, which can change between connections
    /// when the node is reached through an LB/proxy/VPN — a transient
    /// proxy address on a re-report must not silently replace a
    /// previously-good endpoint (all nodes behind one shared proxy would
    /// otherwise converge on the proxy's address). A node whose stored
    /// endpoint is wrong needs an operator-initiated store correction
    /// (no in-tree tooling for that yet); the fabric plan compiler only
    /// fails closed on a NULL or unparseable endpoint — a plausible but
    /// stale one still compiles, and the overlay simply stays
    /// unreachable until the store is corrected.
    ///
    /// FORWARD-COMPAT: first-wins is a stopgap for peer-derived
    /// endpoints. When agents begin self-reporting their underlay
    /// addressing (see `FABRIC_WIREGUARD_PORT` and the fabric plan's
    /// Phase-4 list), an explicit node-reported endpoint MUST win over
    /// the earlier peer-derived pin — routing that path through this
    /// COALESCE would silently keep the stale value. Revisit this
    /// upsert then.
    pub async fn register_fabric_identity(
        &self,
        node_id: &str,
        public_key: &str,
        underlay_mtu: u32,
        underlay_endpoint: Option<&str>,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        // Two registrations may race on the same free fabric IP; the loser
        // trips the unique index (migration 0054) and retries with a
        // freshly observed used-address set. Bounded so a pathological
        // race degrades to a Conflict error instead of looping.
        let mut attempt = 1;
        loop {
            match self
                .register_fabric_identity_once(
                    node_id,
                    public_key,
                    underlay_mtu,
                    underlay_endpoint,
                    audit,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(err)
                    if is_fabric_ip_unique_violation(&err)
                        && attempt < FABRIC_IP_ALLOCATION_ATTEMPTS =>
                {
                    tracing::debug!(
                        node_id,
                        attempt,
                        "fabric transport IP allocation raced with a concurrent registration, retrying"
                    );
                    attempt += 1;
                }
                Err(err) if is_fabric_ip_unique_violation(&err) => {
                    return Err(StoreError::Conflict {
                        entity: "vtep_registry",
                        id: node_id.to_string(),
                        reason:
                            "fabric transport IP allocation lost repeated races in 100.100.0.0/16",
                    });
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Single attempt of [`VtepRepository::register_fabric_identity`].
    async fn register_fabric_identity_once(
        &self,
        node_id: &str,
        public_key: &str,
        underlay_mtu: u32,
        underlay_endpoint: Option<&str>,
        audit: Option<&AuditOutboxInsert>,
    ) -> Result<(), StoreError> {
        let underlay_mtu = if underlay_mtu == 0 {
            None
        } else {
            Some(i64::from(underlay_mtu))
        };

        let mut tx = self.pool.begin().await?;

        // A node without a legacy VTEP row still gets a registry row so the
        // identity is durable; vtep_ip keeps the empty-string sentinel the
        // legacy column requires (NOT NULL) until the fabric path populates
        // fabric_ip below.
        // First-registration-wins for the underlay endpoint: an existing
        // stored value is never replaced by a later (peer-derived, so
        // possibly proxy-shaped) one. The public key and underlay MTU
        // keep their normal upsert semantics.
        sqlx::query(
            r#"INSERT INTO vtep_registry (node_id, vtep_ip, vtep_port, public_key, underlay_mtu, underlay_endpoint, updated_at)
               VALUES (?, '', 4789, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
               ON CONFLICT(node_id) DO UPDATE SET
                   public_key = excluded.public_key,
                   underlay_mtu = excluded.underlay_mtu,
                   underlay_endpoint = COALESCE(vtep_registry.underlay_endpoint, excluded.underlay_endpoint),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"#,
        )
        .bind(node_id)
        .bind(public_key)
        .bind(underlay_mtu)
        .bind(underlay_endpoint)
        .execute(&mut *tx)
        .await?;

        // Allocate the fabric transport IP exactly once per node.
        let existing: Option<Option<String>> =
            sqlx::query_scalar("SELECT fabric_ip FROM vtep_registry WHERE node_id = ?")
                .bind(node_id)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.flatten().is_none() {
            let fabric_ip = Self::next_free_fabric_ip(&mut tx).await?;
            sqlx::query("UPDATE vtep_registry SET fabric_ip = ? WHERE node_id = ?")
                .bind(&fabric_ip)
                .bind(node_id)
                .execute(&mut *tx)
                .await?;
        }

        // #706: the caller's pre-minted audit-outbox row commits with the
        // fabric identity, so enrollment evidence cannot be lost between
        // this commit and the post-hoc audit insert. A failed attempt
        // (including a lost fabric-IP race) rolls the enqueue back with the
        // mutation and the retry enqueues again.
        if let Some(audit) = audit {
            enqueue_on_tx(&mut tx, audit).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Pick the lowest free fabric transport address in 100.100.0.0/16.
    /// Must run inside the caller's transaction so concurrent registrations
    /// have the best chance of observing the same free address; the unique
    /// index on `vtep_registry.fabric_ip` (migration 0054) is the final
    /// arbiter — a loser aborts and the retry wrapper in
    /// [`VtepRepository::register_fabric_identity`] re-runs with a fresh
    /// view of the used set.
    async fn next_free_fabric_ip(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<String, StoreError> {
        let used: Vec<Option<String>> =
            sqlx::query_scalar("SELECT fabric_ip FROM vtep_registry WHERE fabric_ip IS NOT NULL")
                .fetch_all(&mut **tx)
                .await?;
        let mut used_set = std::collections::HashSet::new();
        for ip in used.into_iter().flatten() {
            if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                used_set.insert(u32::from(addr));
            }
        }
        for candidate in FABRIC_IP_FIRST..=FABRIC_IP_LAST {
            if !used_set.contains(&candidate) {
                return Ok(std::net::Ipv4Addr::from(candidate).to_string());
            }
        }
        Err(StoreError::InvalidConfiguration {
            reason: "fabric transport address space 100.100.0.0/16 exhausted".to_string(),
        })
    }

    /// Allocate the next free VNI for a network.
    /// VNI range: 1 to 16777214. Skips VNIs released less than 24 hours ago.
    pub async fn allocate_vni(&self, network_id: &str) -> Result<i32, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Find the next available VNI that is not currently allocated
        // and was not released within the last 24 hours.
        // Try generate_series first (available in SQLite >= 3.8.3 with ENABLE_SERIES),
        // fall back to max+1 approach if not available.
        let next_vni: Option<i32> = match sqlx::query_scalar(
            r#"SELECT MIN(candidate.vni) FROM (
                   SELECT value AS vni FROM generate_series(1, 16777214)
                   WHERE value NOT IN (
                       SELECT vni FROM vni_allocations
                       WHERE released_at IS NULL
                          OR released_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-24 hours')
                   )
                   LIMIT 1
               ) candidate"#,
        )
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(result) => result.flatten(),
            Err(_) => None, // generate_series not available, use fallback
        };

        // Fallback: find max current VNI and use max+1.
        let vni = match next_vni {
            Some(v) => v,
            None => {
                let max_vni: Option<i32> = sqlx::query_scalar(
                    r#"SELECT MAX(vni) FROM vni_allocations
                       WHERE released_at IS NULL
                          OR released_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-24 hours')"#,
                )
                .fetch_optional(&mut *tx)
                .await?
                .flatten();

                max_vni.unwrap_or(0) + 1
            }
        };

        if !(1..=16777214).contains(&vni) {
            return Err(StoreError::InvalidConfiguration {
                reason: "VNI address space exhausted".to_string(),
            });
        }

        sqlx::query(
            r#"INSERT INTO vni_allocations (vni, network_id, allocated_at, binding_generation)
               VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                       COALESCE(
                           (SELECT MAX(binding_generation) FROM vni_allocations WHERE network_id = ?),
                           0
                       ) + 1)"#,
        )
        .bind(vni)
        .bind(network_id)
        .bind(network_id)
        .execute(&mut *tx)
        .await?;

        // Also update the networks table
        sqlx::query("UPDATE networks SET vni = ? WHERE network_id = ?")
            .bind(vni)
            .bind(network_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(vni)
    }

    /// Release a VNI allocation for a network (sets released_at timestamp).
    pub async fn release_vni(&self, network_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            r#"UPDATE vni_allocations
               SET released_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
               WHERE network_id = ? AND released_at IS NULL"#,
        )
        .bind(network_id)
        .execute(&self.pool)
        .await?;

        // Clear the VNI on the network
        sqlx::query("UPDATE networks SET vni = 0 WHERE network_id = ?")
            .bind(network_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Get the current VNI for a network.
    pub async fn get_vni_for_network(&self, network_id: &str) -> Result<Option<i32>, StoreError> {
        let vni: Option<i32> = sqlx::query_scalar(
            "SELECT vni FROM vni_allocations WHERE network_id = ? AND released_at IS NULL",
        )
        .bind(network_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(vni)
    }

    /// Get the binding generation of a network's active VNI allocation
    /// (ADR-021 §8). `Ok(None)` when the network has no active allocation.
    pub async fn get_binding_generation(
        &self,
        network_id: &str,
    ) -> Result<Option<i64>, StoreError> {
        let generation: Option<i64> = sqlx::query_scalar(
            "SELECT binding_generation FROM vni_allocations WHERE network_id = ? AND released_at IS NULL",
        )
        .bind(network_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(generation)
    }
}
