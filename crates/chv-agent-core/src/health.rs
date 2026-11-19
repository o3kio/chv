use crate::state_machine::NodeState;

/// Minimum free disk space before declaring disk pressure (5 GB).
const MIN_DISK_AVAILABLE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Maximum memory usage percentage before declaring memory pressure.
const MAX_MEMORY_USAGE_PERCENT: f64 = 90.0;

/// Indicates whether the host is under resource pressure.
#[derive(Debug, Clone)]
pub struct ResourcePressure {
    pub disk_pressure: bool,
    pub memory_pressure: bool,
    pub disk_available_bytes: u64,
    pub memory_usage_percent: f64,
}

/// Check host resources for disk and memory pressure using sysinfo.
pub fn check_host_resources() -> ResourcePressure {
    use sysinfo::{Disks, System};

    // sysinfo 0.40 made these constructors fallible. Resource pressure is a
    // health signal, not a data path: a transient `/proc` error must degrade
    // to "no pressure" (with a log line), never panic the agent.
    let mut memory_total = 0;
    let mut memory_used = 0;
    match System::new() {
        Ok(mut sys) => {
            sys.refresh_memory();
            memory_total = sys.total_memory();
            memory_used = sys.used_memory();
        }
        Err(err) => {
            tracing::warn!(error = %err, "sysinfo System initialisation failed; memory pressure unknown");
        }
    }
    let memory_pct = if memory_total > 0 {
        (memory_used as f64 / memory_total as f64) * 100.0
    } else {
        0.0
    };

    let disk_available = match Disks::new_with_refreshed_list() {
        Ok(disks) => disks
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))
            .map(|d| d.available_space())
            .unwrap_or(u64::MAX),
        Err(err) => {
            tracing::warn!(error = %err, "sysinfo disk listing failed; disk pressure unknown");
            u64::MAX
        }
    };

    let disk_pressure = disk_available < MIN_DISK_AVAILABLE_BYTES;
    let memory_pressure = memory_pct > MAX_MEMORY_USAGE_PERCENT;

    if disk_pressure {
        tracing::warn!(
            disk_available_bytes = disk_available,
            threshold_bytes = MIN_DISK_AVAILABLE_BYTES,
            "disk pressure detected: available space below threshold"
        );
    }
    if memory_pressure {
        tracing::warn!(
            memory_usage_percent = memory_pct,
            threshold_percent = MAX_MEMORY_USAGE_PERCENT,
            "memory pressure detected: usage above threshold"
        );
    }

    ResourcePressure {
        disk_pressure,
        memory_pressure,
        disk_available_bytes: disk_available,
        memory_usage_percent: memory_pct,
    }
}

#[derive(Debug, Clone, Default)]
pub struct HealthAggregator {
    stord: Option<bool>,
    nwd: Option<bool>,
    core_journal: Option<bool>,
    resource_pressure: Option<ResourcePressure>,
}

impl HealthAggregator {
    pub fn new() -> Self {
        Self {
            stord: None,
            nwd: None,
            core_journal: None,
            resource_pressure: None,
        }
    }

    pub fn update_stord(&mut self, healthy: bool) {
        self.stord = Some(healthy);
    }

    pub fn update_nwd(&mut self, healthy: bool) {
        self.nwd = Some(healthy);
    }

    /// Update the Core journal health signal. In core-managed mode this is fed
    /// from the runtime owner's journal poller; an unhealthy journal means
    /// accepted operations are no longer executing, so the node must not report
    /// fully ready. In legacy mode this is never fed and defaults to healthy.
    pub fn update_core_journal(&mut self, healthy: bool) {
        self.core_journal = Some(healthy);
    }

    /// Update the cached resource pressure state.
    pub fn update_resource_pressure(&mut self, pressure: ResourcePressure) {
        self.resource_pressure = Some(pressure);
    }

    /// Returns whether the host is currently under resource pressure.
    pub fn has_resource_pressure(&self) -> bool {
        self.resource_pressure
            .as_ref()
            .map(|p| p.disk_pressure || p.memory_pressure)
            .unwrap_or(false)
    }

    /// Returns the current resource pressure state, if available.
    pub fn resource_pressure(&self) -> Option<&ResourcePressure> {
        self.resource_pressure.as_ref()
    }

    /// Whether the Core journal health signal permits readiness. Unknown (not
    /// fed, legacy mode) defaults to healthy so legacy nodes are unaffected.
    fn journal_ok(&self) -> bool {
        self.core_journal.unwrap_or(true)
    }

    pub fn derive_node_state(&self, current: NodeState) -> NodeState {
        let stord_ok = self.stord.unwrap_or(false);
        let nwd_ok = self.nwd.unwrap_or(false);

        match current {
            NodeState::Bootstrapping => NodeState::HostReady,
            NodeState::HostReady => {
                if stord_ok {
                    NodeState::StorageReady
                } else {
                    NodeState::HostReady
                }
            }
            NodeState::StorageReady => {
                if !stord_ok {
                    NodeState::Degraded
                } else if nwd_ok {
                    NodeState::NetworkReady
                } else {
                    NodeState::StorageReady
                }
            }
            NodeState::NetworkReady => {
                if stord_ok && nwd_ok && self.journal_ok() {
                    NodeState::TenantReady
                } else {
                    NodeState::Degraded
                }
            }
            NodeState::TenantReady => {
                if stord_ok && nwd_ok && self.journal_ok() && !self.has_resource_pressure() {
                    NodeState::TenantReady
                } else {
                    NodeState::Degraded
                }
            }
            NodeState::Degraded => {
                if stord_ok && nwd_ok && self.journal_ok() && !self.has_resource_pressure() {
                    NodeState::TenantReady
                } else {
                    NodeState::Degraded
                }
            }
            NodeState::Failed => {
                if stord_ok && nwd_ok && self.journal_ok() {
                    NodeState::HostReady
                } else if stord_ok || nwd_ok {
                    NodeState::Degraded
                } else {
                    current
                }
            }
            NodeState::Draining | NodeState::Maintenance | NodeState::Discovered => current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_both_ready_goes_tenant_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        assert_eq!(
            h.derive_node_state(NodeState::HostReady),
            NodeState::StorageReady
        );
    }

    #[test]
    fn health_stord_down_degrades() {
        let mut h = HealthAggregator::new();
        h.update_stord(false);
        h.update_nwd(true);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_nwd_down_degrades() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(false);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_both_down_degrades() {
        let mut h = HealthAggregator::new();
        h.update_stord(false);
        h.update_nwd(false);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_from_host_ready_partial() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(false);
        assert_eq!(
            h.derive_node_state(NodeState::HostReady),
            NodeState::StorageReady
        );
    }

    #[test]
    fn health_from_host_ready_nwd_only() {
        let mut h = HealthAggregator::new();
        h.update_stord(false);
        h.update_nwd(true);
        assert_eq!(
            h.derive_node_state(NodeState::HostReady),
            NodeState::HostReady
        );
    }

    #[test]
    fn health_bootstrap_progresses_one_step_at_a_time() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);

        assert_eq!(
            h.derive_node_state(NodeState::Bootstrapping),
            NodeState::HostReady
        );
        assert_eq!(
            h.derive_node_state(NodeState::HostReady),
            NodeState::StorageReady
        );
        assert_eq!(
            h.derive_node_state(NodeState::StorageReady),
            NodeState::NetworkReady
        );
        assert_eq!(
            h.derive_node_state(NodeState::NetworkReady),
            NodeState::TenantReady
        );
    }

    #[test]
    fn health_failed_recover_to_host_ready_when_both_up() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        assert_eq!(h.derive_node_state(NodeState::Failed), NodeState::HostReady);
    }

    #[test]
    fn health_failed_recover_to_degraded_when_partial() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(false);
        assert_eq!(h.derive_node_state(NodeState::Failed), NodeState::Degraded);
    }

    #[test]
    fn health_failed_stays_failed_when_both_down() {
        let mut h = HealthAggregator::new();
        h.update_stord(false);
        h.update_nwd(false);
        assert_eq!(h.derive_node_state(NodeState::Failed), NodeState::Failed);
    }

    #[test]
    fn health_journal_unhealthy_prevents_tenant_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(false);
        assert_eq!(
            h.derive_node_state(NodeState::NetworkReady),
            NodeState::Degraded
        );
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_journal_healthy_keeps_tenant_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(true);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::TenantReady
        );
    }

    #[test]
    fn health_journal_unfed_defaults_to_healthy() {
        // Legacy mode never feeds core_journal; it must be a no-op.
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::TenantReady
        );
    }

    #[test]
    fn health_failed_with_bad_journal_recovers_to_degraded_not_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(false);
        assert_eq!(h.derive_node_state(NodeState::Failed), NodeState::Degraded);
    }

    // #562: the pressure arms the main-loop wiring feeds — previously
    // reachable only in tests because nothing called
    // `update_resource_pressure` (the dormant-gauges finding).
    fn pressure(disk: bool, memory: bool) -> ResourcePressure {
        ResourcePressure {
            disk_pressure: disk,
            memory_pressure: memory,
            disk_available_bytes: 0,
            memory_usage_percent: 0.0,
        }
    }

    #[test]
    fn health_resource_pressure_degrades_tenant_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(true);
        h.update_resource_pressure(pressure(false, true));
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_disk_pressure_alone_degrades_tenant_ready() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_resource_pressure(pressure(true, false));
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_resource_pressure_cleared_recovers_tenant_ready() {
        // The recovery arm, same-instance: pressure degrades a TenantReady
        // node, and clearing the input (the flap's floor, not a ratchet —
        // `update_resource_pressure` replaces the value) recovers it.
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(true);
        h.update_resource_pressure(pressure(true, false));
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::Degraded
        );
        h.update_resource_pressure(pressure(false, false));
        assert_eq!(
            h.derive_node_state(NodeState::Degraded),
            NodeState::TenantReady
        );
    }

    #[test]
    fn health_resource_pressure_blocks_degraded_recovery_while_present() {
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        h.update_core_journal(true);
        h.update_resource_pressure(pressure(false, true));
        assert_eq!(
            h.derive_node_state(NodeState::Degraded),
            NodeState::Degraded
        );
    }

    #[test]
    fn health_resource_pressure_unfed_is_no_op() {
        // The pre-#562 posture: a caller that never feeds the pressure
        // input (none exists in-tree today, but the aggregator is a
        // library surface) keeps today's behavior — no phantom pressure.
        let mut h = HealthAggregator::new();
        h.update_stord(true);
        h.update_nwd(true);
        assert_eq!(
            h.derive_node_state(NodeState::TenantReady),
            NodeState::TenantReady
        );
        assert!(!h.has_resource_pressure());
        assert!(h.resource_pressure().is_none());
    }
}
