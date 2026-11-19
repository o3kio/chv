//! Long-lived Linux node collector (the sysinfo CPU repair, prompt 01
//! task 2).
//!
//! **The defect this fixes:** the previous path instantiated a fresh
//! `sysinfo::System` per scrape and read `global_cpu_usage()` after a
//! single `refresh_cpu_usage()`. On a fresh `System` the first refresh
//! computes usage **since boot**, so every scrape reported the host's
//! lifetime average — a constant-ish number that is not the current load.
//! sysinfo needs two refreshes separated by
//! [`System::MINIMUM_CPU_UPDATE_INTERVAL`] to measure an interval.
//!
//! **The fix:** one long-lived [`NodeOsCollector`] retains its `System`
//! across cycles. The first snapshot reports `cpu_capacity_ratio: None`
//! (`insufficient_samples` — no valid interval exists yet); later
//! snapshots report the busy fraction over the elapsed interval. If a
//! cycle arrives too soon after the previous refresh, the last valid
//! reading is retained (with its own observation timestamp) rather than
//! recomputing a bogus interval.
//!
//! **CPU busy and core normalization:** sysinfo's `global_cpu_usage()` is
//! the busy-time fraction across **all logical CPUs** (0–100, already
//! averaged over cores); `cpu_capacity_ratio` divides by 100 to the
//! contract's 0–1 ratio. Per-core normalization is therefore not applied
//! again here — a node's capacity ratio of 0.25 means one quarter of all
//! logical CPU time was busy, regardless of core count.

use std::fs;
use std::path::Path;
use std::time::Instant;

use sysinfo::{Disks, System};

/// One node-level observation cycle. `None` fields are unavailable this
/// cycle — never zero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeOsSnapshot {
    /// When the non-CPU fields were observed (Unix ms).
    pub observed_at_ms: u64,
    /// Busy fraction of all logical CPUs over the measurement interval
    /// (0–1). `None` until a valid interval exists (first cycle) —
    /// `insufficient_samples`, not zero.
    pub cpu_capacity_ratio: Option<f64>,
    /// When `cpu_capacity_ratio` was actually observed (Unix ms) — can
    /// lag `observed_at_ms` when a cycle arrived inside sysinfo's minimum
    /// interval and the last valid reading was retained.
    pub cpu_observed_at_ms: u64,
    /// Linux load averages (not CPU percent). `None` when `/proc/loadavg`
    /// could not be read or parsed.
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub memory_total_bytes: u64,
    /// Linux `MemAvailable`.
    pub memory_available_bytes: u64,
    pub swap_used_bytes: u64,
    /// Root filesystem sizes. `None` when the root mount was not found.
    pub root_fs_total_bytes: Option<u64>,
    pub root_fs_available_bytes: Option<u64>,
}

/// Long-lived node collector. One instance must be retained across cycles
/// (e.g. held by the sampler task or the metrics server) — that retention
/// is the CPU-interval fix. Not `Clone`: cloning would fork the retained
/// CPU baseline and reintroduce the fresh-system defect.
pub struct NodeOsCollector {
    /// `None` while sysinfo initialisation keeps failing. sysinfo 0.40 made
    /// `System::new()` fallible, and this collector must stay infallible so
    /// a transient `/proc` error cannot take the agent metrics listener
    /// down. A snapshot taken while degraded reports zeroed memory/disk
    /// values and no CPU ratio, and every snapshot retries initialisation.
    sys: Option<System>,
    last_cpu_refresh: Option<Instant>,
    last_cpu_ratio: Option<f64>,
    last_cpu_observed_ms: u64,
}

impl NodeOsCollector {
    /// A collector with no retained interval; the first [`Self::snapshot`]
    /// reports `cpu_capacity_ratio: None`.
    pub fn new() -> Self {
        NodeOsCollector {
            sys: System::new().map_err(|err| {
                tracing::warn!(error = %err, "sysinfo System initialisation failed; metrics degraded")
            }).ok(),
            last_cpu_refresh: None,
            last_cpu_ratio: None,
            last_cpu_observed_ms: 0,
        }
    }

    /// Take one observation. Refreshes CPU (interval-guarded), memory and
    /// swap on the retained `System`, lists disks, and reads
    /// `/proc/loadavg`.
    pub fn snapshot(&mut self) -> NodeOsSnapshot {
        let now_ms = unix_now_ms();

        // Retry while degraded: an init failure is treated as transient and
        // must not disable host metrics for the agent's whole lifetime.
        if self.sys.is_none() {
            self.sys = System::new()
                .map_err(
                    |err| tracing::debug!(error = %err, "sysinfo System re-initialisation failed"),
                )
                .ok();
        }

        let mut memory_total_bytes = 0;
        let mut memory_available_bytes = 0;
        let mut swap_used_bytes = 0;
        if let Some(sys) = self.sys.as_mut() {
            // CPU: refresh only when sysinfo's minimum interval has elapsed
            // since the last refresh — refreshing inside the interval would
            // reset the baseline and make the next "interval" shorter than
            // claimed. When not eligible, retain the last valid reading
            // (with its own timestamp).
            let eligible = self
                .last_cpu_refresh
                .map(|t| t.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)
                .unwrap_or(true);
            if eligible {
                sys.refresh_cpu_usage();
                // A fresh System's first refresh is the since-boot average;
                // only a refresh on a System that already saw a previous
                // refresh measures the elapsed interval.
                let measured_interval = self.last_cpu_refresh.is_some();
                let usage = sys.global_cpu_usage();
                if measured_interval && usage.is_finite() && usage >= 0.0 {
                    self.last_cpu_ratio = Some((usage as f64 / 100.0).clamp(0.0, 1.0));
                    self.last_cpu_observed_ms = now_ms;
                }
                self.last_cpu_refresh = Some(Instant::now());
            }

            sys.refresh_memory();
            memory_total_bytes = sys.total_memory();
            memory_available_bytes = sys.available_memory();
            swap_used_bytes = sys.used_swap();
        }

        // Disks carry no interval semantics; a refreshed list per cycle is
        // the documented per-scrape precedent. The root mount is the
        // filesystem the pressure checks and dashboards reason about.
        let (root_fs_total_bytes, root_fs_available_bytes) = match Disks::new_with_refreshed_list()
        {
            Ok(disks) => disks
                .iter()
                .find(|d| d.mount_point() == Path::new("/"))
                .map(|d| (Some(d.total_space()), Some(d.available_space())))
                .unwrap_or((None, None)),
            Err(err) => {
                tracing::debug!(error = %err, "sysinfo disk listing failed");
                (None, None)
            }
        };

        let (load1, load5, load15) = read_loadavg();

        NodeOsSnapshot {
            observed_at_ms: now_ms,
            cpu_capacity_ratio: self.last_cpu_ratio,
            cpu_observed_at_ms: self.last_cpu_observed_ms,
            load1,
            load5,
            load15,
            memory_total_bytes,
            memory_available_bytes,
            swap_used_bytes,
            root_fs_total_bytes,
            root_fs_available_bytes,
        }
    }

    /// The retained CPU reading and when it was observed (for staleness
    /// checks by consumers holding a snapshot).
    pub fn retained_cpu(&self) -> (Option<f64>, u64) {
        (self.last_cpu_ratio, self.last_cpu_observed_ms)
    }
}

impl Default for NodeOsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Read and parse `/proc/loadavg`.
pub fn read_loadavg() -> (Option<f64>, Option<f64>, Option<f64>) {
    parse_loadavg(&fs::read_to_string("/proc/loadavg").unwrap_or_default())
}

/// Parse the first three fields of a `/proc/loadavg` line
/// (`"0.52 0.58 0.59 1/867 12345"`).
pub fn parse_loadavg(line: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut it = line.split_whitespace();
    let one = it.next().and_then(|s| s.parse::<f64>().ok());
    let five = it.next().and_then(|s| s.parse::<f64>().ok());
    let fifteen = it.next().and_then(|s| s.parse::<f64>().ok());
    (one, five, fifteen)
}

pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_snapshot_has_no_cpu_interval() {
        // The fresh-system defect, made impossible to reintroduce
        // silently: the very first cycle cannot report a CPU ratio.
        let mut c = NodeOsCollector::new();
        let s = c.snapshot();
        assert_eq!(s.cpu_capacity_ratio, None);
        // Memory and load fields are populated on a real host.
        assert!(s.memory_total_bytes > 0);
        assert!(s.load1.is_some());
    }

    #[test]
    fn later_snapshots_measure_an_interval() {
        let mut c = NodeOsCollector::new();
        let first = c.snapshot();
        assert_eq!(first.cpu_capacity_ratio, None);
        // Wait past sysinfo's minimum interval, then snapshot again: a
        // real interval measurement exists now (any 0..1 value).
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let second = c.snapshot();
        let ratio = second.cpu_capacity_ratio.expect("interval measurement");
        assert!((0.0..=1.0).contains(&ratio));
        assert!(second.cpu_observed_at_ms >= second.observed_at_ms.saturating_sub(1_000));
    }

    #[test]
    fn rapid_cycle_retains_last_reading() {
        let mut c = NodeOsCollector::new();
        c.snapshot();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let measured = c.snapshot().cpu_capacity_ratio;
        // An immediate extra cycle inside the minimum interval must not
        // fabricate a new interval: it retains the same reading.
        let immediate = c.snapshot();
        assert_eq!(immediate.cpu_capacity_ratio, measured);
    }

    #[test]
    fn loadavg_parses() {
        let (a, b, c) = parse_loadavg("0.52 0.58 0.59 1/867 12345\n");
        assert_eq!((a, b, c), (Some(0.52), Some(0.58), Some(0.59)));
        let (a, b, c) = parse_loadavg("");
        assert_eq!((a, b, c), (None, None, None));
        let (a, b, c) = parse_loadavg("garbage");
        assert_eq!((a, b, c), (None, None, None));
    }
}
