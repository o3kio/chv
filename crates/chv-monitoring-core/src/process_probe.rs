//! Read-only `/proc/<pid>` probes for the identity-fenced VMM process.
//!
//! These probes read **host** process state — never guest paths, never
//! anything writable. Every function takes an explicit `proc_root` so
//! tests run against crafted trees and production callers pass `/proc`.
//!
//! Units (proc(5), stable Linux userspace ABI):
//! - `stat` `utime`/`stime` are in clock ticks of `CLOCK_TICKS_HZ` (100/s
//!   — `USER_HZ` is 100 on every Linux ABI regardless of kernel `HZ`);
//! - `stat` `starttime` (field 22) is in the same ticks since boot — the
//!   process-identity fence the runtime already uses;
//! - `status` `VmRSS` is in kB (kernel-parsed, no page-size dependency).

use std::fs;
use std::path::{Path, PathBuf};

/// `/proc/<pid>/stat` time units — always 100/s in the Linux userspace
/// ABI, independent of the kernel's internal `HZ`.
pub const CLOCK_TICKS_HZ: u64 = 100;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("proc probe I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("process {0} is gone")]
    ProcessGone(u32),
}

/// Parsed `/proc/<pid>/stat` — the fields monitoring needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcStat {
    pub pid: u32,
    /// User CPU time in clock ticks.
    pub utime_ticks: u64,
    /// System CPU time in clock ticks.
    pub stime_ticks: u64,
    /// Process start time in clock ticks since boot — the
    /// pid-recycling fence.
    pub start_ticks: u64,
}

impl ProcStat {
    /// Total user+system CPU time in seconds (exact in ticks; f64 only
    /// at this conversion boundary).
    pub fn cpu_seconds(&self) -> f64 {
        (self.utime_ticks + self.stime_ticks) as f64 / CLOCK_TICKS_HZ as f64
    }

    /// The identity-epoch string for sample/delta fencing: a recycled pid
    /// has different start ticks and therefore a different epoch.
    pub fn identity_epoch(&self) -> String {
        format!("pid-{}-start-{}", self.pid, self.start_ticks)
    }
}

fn proc_file(proc_root: &Path, pid: u32, name: &str) -> PathBuf {
    proc_root.join(pid.to_string()).join(name)
}

/// Read `/proc/<pid>/stat`. The comm field may contain spaces and
/// parentheses, so parsing starts after the **last** `)`.
pub fn read_proc_stat(proc_root: &Path, pid: u32) -> Result<ProcStat, ProbeError> {
    let raw =
        fs::read_to_string(proc_file(proc_root, pid, "stat")).map_err(|e| classify_io(e, pid))?;
    let rest = raw
        .rsplit_once(')')
        .ok_or(ProbeError::Malformed("stat: no comm terminator"))?
        .1;
    // Fields after the comm, 0-based: [0]=state(3) ... [11]=utime(14),
    // [12]=stime(15), [19]=starttime(22).
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return Err(ProbeError::Malformed("stat: too few fields"));
    }
    let parse = |i: usize, what: &'static str| -> Result<u64, ProbeError> {
        fields[i]
            .parse::<u64>()
            .map_err(|_| ProbeError::Malformed(what))
    };
    // The pid is the first token of the whole line (before the comm).
    let pid_field = raw
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(pid);
    Ok(ProcStat {
        pid: pid_field,
        utime_ticks: parse(11, "stat: utime")?,
        stime_ticks: parse(12, "stat: stime")?,
        start_ticks: parse(19, "stat: starttime")?,
    })
}

/// Read the process's resident set size in bytes from
/// `/proc/<pid>/status` (`VmRSS`, kB). The kernel-parsed line avoids the
/// page-size dependency of `statm` (which sysinfo no longer
/// exposes). This is the **host-accounted** memory of the process —
/// explicitly not the guest working set (see the native monitoring
/// spec's memory split).
pub fn read_rss_bytes(proc_root: &Path, pid: u32) -> Result<u64, ProbeError> {
    let raw =
        fs::read_to_string(proc_file(proc_root, pid, "status")).map_err(|e| classify_io(e, pid))?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .map_err(|_| ProbeError::Malformed("status: VmRSS"))?;
            return Ok(kb.saturating_mul(1024));
        }
    }
    Err(ProbeError::Malformed("status: no VmRSS"))
}

/// Read the host boot id (`/proc/sys/kernel/random/boot_id`) — the boot
/// half of every counter epoch.
pub fn read_boot_id(proc_root: &Path) -> Result<String, ProbeError> {
    let raw = fs::read_to_string(proc_root.join("sys/kernel/random/boot_id"))?;
    let id = raw.trim().to_string();
    if id.is_empty() {
        return Err(ProbeError::Malformed("boot_id: empty"));
    }
    Ok(id)
}

fn classify_io(e: std::io::Error, pid: u32) -> ProbeError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ProbeError::ProcessGone(pid),
        _ => ProbeError::Io(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(dir: &Path, pid: u32, stat: &str, status_rss_kb: u64) {
        let pdir = dir.join(pid.to_string());
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join("stat"), stat).unwrap();
        fs::write(
            pdir.join("status"),
            format!("Name:\tproc\nVmRSS:\t  {status_rss_kb} kB\n"),
        )
        .unwrap();
    }

    #[test]
    fn parses_stat_with_spaces_and_parens_in_comm() {
        let tmp = tempfile::tempdir().unwrap();
        // comm containing both a space and a parenthesis; field values
        // chosen so utime/stime/starttime are unambiguous.
        fake_proc(
            tmp.path(),
            42,
            "42 (cloud hypervis) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 987654 21 22",
            50,
        );
        let stat = read_proc_stat(tmp.path(), 42).unwrap();
        // fields after comm: [0]=R ... [11]=11(utime) [12]=12(stime)
        // [19]=19(starttime) — the values sit at their index positions.
        assert_eq!(stat.utime_ticks, 11);
        assert_eq!(stat.stime_ticks, 12);
        assert_eq!(stat.start_ticks, 19);
        assert_eq!(stat.pid, 42);
        assert!((stat.cpu_seconds() - 0.23).abs() < 1e-9);
        assert_eq!(stat.identity_epoch(), "pid-42-start-19");
    }

    #[test]
    fn rss_reads_from_status() {
        let tmp = tempfile::tempdir().unwrap();
        fake_proc(
            tmp.path(),
            7,
            "7 (x) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 5 21 22",
            2048,
        );
        assert_eq!(read_rss_bytes(tmp.path(), 7).unwrap(), 2048 * 1024);
    }

    #[test]
    fn gone_process_is_reported_as_gone() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_proc_stat(tmp.path(), 999),
            Err(ProbeError::ProcessGone(999))
        ));
        assert!(matches!(
            read_rss_bytes(tmp.path(), 999),
            Err(ProbeError::ProcessGone(999))
        ));
    }

    #[test]
    fn malformed_stat_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        fake_proc(tmp.path(), 1, "no terminator", 1);
        assert!(matches!(
            read_proc_stat(tmp.path(), 1),
            Err(ProbeError::Malformed(_))
        ));
        fake_proc(tmp.path(), 2, "2 (x) R", 1);
        assert!(matches!(
            read_proc_stat(tmp.path(), 2),
            Err(ProbeError::Malformed(_))
        ));
    }

    #[test]
    fn boot_id_reads_from_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sys/kernel/random");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("boot_id"),
            "00000000-0000-4000-8000-000000000001\n",
        )
        .unwrap();
        assert_eq!(
            read_boot_id(tmp.path()).unwrap(),
            "00000000-0000-4000-8000-000000000001"
        );
    }

    #[test]
    fn real_proc_root_parses_self() {
        // The real /proc must parse this process's own stat and status (VmRSS).
        let stat = read_proc_stat(Path::new("/proc"), std::process::id()).unwrap();
        assert_eq!(stat.pid, std::process::id());
        assert!(read_rss_bytes(Path::new("/proc"), std::process::id()).unwrap() > 0);
        assert!(read_boot_id(Path::new("/proc")).unwrap().len() == 36);
    }
}
