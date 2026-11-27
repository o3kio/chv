//! Privileged host-safety regression test for the CHV firewall (#227).
//!
//! Proves on a real Linux host that CHV firewall policy is confined to
//! CHV-owned guest traffic:
//!
//! - unrelated host traffic (container/CNI-style, host-stack INPUT and OUTPUT
//!   paths: a namespace talking to a host address and the host replying into a
//!   namespace) keeps working after a CHV policy is applied. This fails against
//!   the old host-wide `policy drop` implementation, because the old code
//!   created `input`/`forward`/`output` chains with `policy drop` and no
//!   guards, dropping ALL host traffic on the host netns — including this
//!   unrelated INPUT/OUTPUT traffic;
//! - CHV-owned guest traffic (bridge + enslaved member) is default-dropped with
//!   an empty policy;
//! - an allow rule restores guest connectivity;
//! - re-applying policy is idempotent and leaves unrelated traffic untouched.
//!
//! We deliberately exercise host INPUT/OUTPUT (not a forward-through-host path)
//! for isolated "unrelated" traffic: hosts commonly run their own
//! `hook forward policy drop` firewall (Docker/CNI/kube-proxy), which would
//! drop any forwarded-unrelated path regardless of CHV and make the test
//! non-isolating. Host-stack INPUT/OUTPUT hooks are precisely the paths the old
//! CHV bug clobbered (SSH, kubelet, health checks, container<->host).
//!
//! Requires root and `nft`/`ip`; skipped (trivially passes) otherwise. Run from
//! the CI-less local host with:
//!
//! ```text
//! cargo test -p chv-nwd-core --no-run
//! sudo -E $(find target/debug/deps -maxdepth 1 -name 'host_safety-*' -type f -executable |
//!     head -1) --ignored --exact confines_policy_to_chv_owned_traffic
//! ```
//!
//! Set `HOST_SAFETY_DUMP_PATH` to a file to persist the CHV nftables ruleset
//! (evidence capture); the file is appended per dump section.

use std::process::Command;

// Unrelated host-stack paths (host is 10.200.x.1 on each veth host-end).
const UNREL_A_SUBNET: &str = "10.200.1.0/24";
const UNREL_A_HOST_IP: &str = "10.200.1.1"; // host-side of veth pair A (INPUT path)
const UNREL_A_NS_IP: &str = "10.200.1.2"; // in ns us-a
const UNREL_B_SUBNET: &str = "10.200.2.0/24";
const UNREL_B_HOST_IP: &str = "10.200.2.1"; // host-side of veth pair B (OUTPUT path)
const UNREL_B_NS_IP: &str = "10.200.2.2"; // in ns us-b

// CHV-owned guest boundary.
const GUEST_GW: &str = "10.201.0.1";
const GUEST_IP: &str = "10.201.0.2";

fn sh(args: &[&str]) -> Vec<u8> {
    let out = Command::new(args[0]).args(&args[1..]).output().unwrap();
    assert!(
        out.status.success(),
        "command failed ({status}): {args:?}\nstderr: {stderr}",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn ns(ns: &str, args: &[&str]) -> Vec<u8> {
    let mut full = vec!["ip", "netns", "exec", ns, "ip"];
    full.extend_from_slice(args);
    sh(&full)
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn nft_available() -> bool {
    Command::new("nft")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Cleans up every resource the test created, best-effort.
///
/// Created BEFORE any setup command so a panic in the middle of setup still
/// unwinds through this guard and removes partial state (#227 S6).
struct Cleanup {
    namespaces: Vec<String>,
    links: Vec<String>,
    nft_table: String,
}

impl Cleanup {
    fn new(u: &str) -> Self {
        Cleanup {
            namespaces: vec![format!("us-a{u}"), format!("us-b{u}"), format!("gs{u}")],
            links: vec![
                // Host-side ends of the unrelated veth pairs.
                format!("ha{u}"),
                format!("hb{u}"),
                // CHV-owned bridge and its host-side member end (gg lives in gs).
                format!("brhs{u}"),
                format!("gh{u}"),
            ],
            nft_table: format!("chvhs-{u}"),
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("nft")
            .args(["delete", "table", "inet", &self.nft_table])
            .output();
        for ns_name in &self.namespaces {
            let _ = Command::new("ip").args(["netns", "del", ns_name]).output();
        }
        for link in &self.links {
            let _ = Command::new("ip").args(["link", "del", link]).output();
        }
    }
}

/// Ping from a namespace with retries (ARP/ND resolution on first packets can
/// lose the first probe on freshly configured veths/routes).
fn ns_ping(ns_name: &str, dst: &str) -> bool {
    for _ in 0..3 {
        let ok = Command::new("ip")
            .args([
                "netns", "exec", ns_name, "ping", "-c", "1", "-W", "1", "-q", dst,
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

/// Ping from the host netns with retries (OUTPUT path into a namespace).
fn host_ping(dst: &str) -> bool {
    for _ in 0..3 {
        let ok = Command::new("ping")
            .args(["-c", "1", "-W", "1", "-q", dst])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

/// Optionally persist the CHV nftables table for evidence capture.
/// Controlled by HOST_SAFETY_DUMP_PATH (append to file); without it the dump
/// is logged at debug level. Not set in CI.
fn dump_table(table: &str) {
    let out = Command::new("nft")
        .args(["list", "table", "inet", table])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|_| "<nft list failed>".to_string());
    let section = format!("--- nft table {table} ---\n{out}\n");
    match std::env::var("HOST_SAFETY_DUMP_PATH") {
        Ok(path) if !path.is_empty() => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = f.write_all(section.as_bytes());
            }
        }
        _ => tracing::debug!(dump = %section, "host-safety nft dump"),
    }
}

#[tokio::test]
#[ignore = "requires root + real nftables on a Linux host; run locally via the fixture doc example"]
async fn confines_policy_to_chv_owned_traffic() {
    if !is_root() || !nft_available() {
        tracing::info!("SKIP: host_safety needs root + nft; run via sudo outside CI");
        return;
    }

    // Collision-averse run suffix: 16-bit pid + 16-bit sub-second clock kept
    // short so every generated interface name stays within Linux IFNAMSIZ
    // (15 chars). The cleanup guard is created here, before any setup command,
    // so a mid-setup panic still removes partial state.
    let u = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:04x}{:04x}", std::process::id() & 0xffff, nanos & 0xffff)
    };
    const IFNAMSIZ: usize = 15;
    let longest_name = format!("brhs{u}");
    assert!(
        longest_name.len() <= IFNAMSIZ,
        "generated interface name {longest_name} would exceed Linux IFNAMSIZ"
    );
    let cleanup = Cleanup::new(&u);

    // --- UNRELATED host-stack traffic ---
    // us-a talks to the host (INPUT hook, iifname=ha); the host replies into
    // us-b (OUTPUT hook, oifname=hb). None of ha/hb/ua/ub are CHV-owned.
    sh(&["ip", "netns", "add", &format!("us-a{u}")]);
    sh(&["ip", "netns", "add", &format!("us-b{u}")]);

    sh(&[
        "ip",
        "link",
        "add",
        &format!("ha{u}"),
        "type",
        "veth",
        "peer",
        "name",
        &format!("ua{u}"),
    ]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("ua{u}"),
        "netns",
        &format!("us-a{u}"),
    ]);
    sh(&[
        "ip",
        "addr",
        "add",
        &format!(
            "{UNREL_A_HOST_IP}/{}",
            UNREL_A_SUBNET.split('/').nth(1).unwrap()
        ),
        "dev",
        &format!("ha{u}"),
    ]);
    sh(&["ip", "link", "set", &format!("ha{u}"), "up"]);

    sh(&[
        "ip",
        "link",
        "add",
        &format!("hb{u}"),
        "type",
        "veth",
        "peer",
        "name",
        &format!("ub{u}"),
    ]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("ub{u}"),
        "netns",
        &format!("us-b{u}"),
    ]);
    sh(&[
        "ip",
        "addr",
        "add",
        &format!(
            "{UNREL_B_HOST_IP}/{}",
            UNREL_B_SUBNET.split('/').nth(1).unwrap()
        ),
        "dev",
        &format!("hb{u}"),
    ]);
    sh(&["ip", "link", "set", &format!("hb{u}"), "up"]);

    ns(&format!("us-a{u}"), &["link", "set", "lo", "up"]);
    ns(
        &format!("us-a{u}"),
        &[
            "addr",
            "add",
            &format!(
                "{UNREL_A_NS_IP}/{}",
                UNREL_A_SUBNET.split('/').nth(1).unwrap()
            ),
            "dev",
            &format!("ua{u}"),
        ],
    );
    ns(
        &format!("us-a{u}"),
        &["link", "set", &format!("ua{u}"), "up"],
    );

    ns(&format!("us-b{u}"), &["link", "set", "lo", "up"]);
    ns(
        &format!("us-b{u}"),
        &[
            "addr",
            "add",
            &format!(
                "{UNREL_B_NS_IP}/{}",
                UNREL_B_SUBNET.split('/').nth(1).unwrap()
            ),
            "dev",
            &format!("ub{u}"),
        ],
    );
    ns(
        &format!("us-b{u}"),
        &["link", "set", &format!("ub{u}"), "up"],
    );

    // --- CHV guest boundary (bridge + enslaved member) ---
    sh(&["ip", "link", "add", &format!("brhs{u}"), "type", "bridge"]);
    sh(&[
        "ip",
        "addr",
        "add",
        &format!("{GUEST_GW}/24"),
        "dev",
        &format!("brhs{u}"),
    ]);
    sh(&["ip", "link", "set", &format!("brhs{u}"), "up"]);
    sh(&[
        "ip",
        "link",
        "add",
        &format!("gh{u}"),
        "type",
        "veth",
        "peer",
        "name",
        &format!("gg{u}"),
    ]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("gh{u}"),
        "master",
        &format!("brhs{u}"),
    ]);
    sh(&["ip", "link", "set", &format!("gh{u}"), "up"]);
    sh(&["ip", "netns", "add", &format!("gs{u}")]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("gg{u}"),
        "netns",
        &format!("gs{u}"),
    ]);
    ns(&format!("gs{u}"), &["link", "set", "lo", "up"]);
    ns(
        &format!("gs{u}"),
        &[
            "addr",
            "add",
            &format!("{GUEST_IP}/24"),
            "dev",
            &format!("gg{u}"),
        ],
    );
    ns(&format!("gs{u}"), &["link", "set", &format!("gg{u}"), "up"]);

    // Baseline: unrelated host-stack paths and the guest path all work before
    // any CHV policy is applied.
    assert!(
        ns_ping(&format!("us-a{u}"), UNREL_A_HOST_IP),
        "unrelated host INPUT path (ns-a -> host) must work at baseline"
    );
    assert!(
        host_ping(UNREL_B_NS_IP),
        "unrelated host OUTPUT path (host -> ns-b) must work at baseline"
    );
    assert!(
        ns_ping(&format!("gs{u}"), GUEST_GW),
        "guest->gateway must work at baseline"
    );

    // CHV-owned interface set: the topology bridge + its enslaved member.
    let owned = vec![format!("brhs{u}"), format!("gh{u}")];
    let table = format!("chvhs-{u}");

    // 1) EMPTY policy -> default-deny inside the boundary; unrelated unaffected.
    chv_nwd_core::firewall::apply_firewall_rules(&table, &owned, b"[]")
        .await
        .unwrap();
    dump_table(&table);
    assert!(
        ns_ping(&format!("us-a{u}"), UNREL_A_HOST_IP),
        "REGRESSION (#227): unrelated host INPUT traffic must survive a CHV empty \
         policy (old code dropped it via host-wide policy drop on the input hook)"
    );
    assert!(
        host_ping(UNREL_B_NS_IP),
        "REGRESSION (#227): unrelated host OUTPUT traffic must survive a CHV empty \
         policy (old code dropped it via host-wide policy drop on the output hook)"
    );
    assert!(
        !ns_ping(&format!("gs{u}"), GUEST_GW),
        "guest->gateway must be default-DENIED inside the CHV boundary under an empty policy"
    );

    // 2) ALLOW policy -> guest connectivity restored; unrelated still untouched.
    let allow = br#"[{"direction":"inbound","protocol":"icmp","source_cidr":"10.201.0.0/24","action":"accept"}]"#;
    chv_nwd_core::firewall::apply_firewall_rules(&table, &owned, allow)
        .await
        .unwrap();
    dump_table(&table);
    assert!(
        ns_ping(&format!("gs{u}"), GUEST_GW),
        "guest->gateway must pass once an inbound allow rule matches"
    );
    assert!(
        ns_ping(&format!("us-a{u}"), UNREL_A_HOST_IP),
        "unrelated host INPUT traffic must remain unaffected by CHV allow policy"
    );
    assert!(
        host_ping(UNREL_B_NS_IP),
        "unrelated host OUTPUT traffic must remain unaffected by CHV allow policy"
    );

    // 3) Re-apply EMPTY policy -> idempotent, guest default-deny re-established.
    chv_nwd_core::firewall::apply_firewall_rules(&table, &owned, b"[]")
        .await
        .unwrap();
    dump_table(&table);
    assert!(
        ns_ping(&format!("us-a{u}"), UNREL_A_HOST_IP),
        "unrelated host INPUT traffic must remain unaffected after a re-apply"
    );
    assert!(
        host_ping(UNREL_B_NS_IP),
        "unrelated host OUTPUT traffic must remain unaffected after a re-apply"
    );
    assert!(
        !ns_ping(&format!("gs{u}"), GUEST_GW),
        "re-applied empty policy must re-establish CHV default-deny"
    );

    drop(cleanup);
}

/// Dump a single chain's ruleset ("" if the chain/table is absent).
fn chain_dump(table: &str, chain: &str) -> String {
    Command::new("nft")
        .args(["list", "chain", "inet", table, chain])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Privileged proof that a service exposure's forward-accept rule survives a
/// firewall apply (re-asserted by the executor) and is removed on withdraw
/// (SHOULD #3 from the #227 review).
///
/// Requires root + `nft`/`ip`; skipped on CI. Run against the built test
/// binary under sudo with `--ignored --exact exposure_survives_firewall_apply`.
#[tokio::test]
#[ignore = "requires root + real nftables on a Linux host; run locally via the fixture doc example"]
async fn exposure_survives_firewall_apply() {
    use chv_nwd_core::executor::{LinuxExecutor, NetworkExecutor};
    use std::path::PathBuf;

    if !is_root() || !nft_available() {
        tracing::info!("SKIP: exposure_survives_firewall_apply needs root + nft");
        return;
    }

    let u = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:04x}{:04x}", std::process::id() & 0xffff, nanos & 0xffff)
    };
    const IFNAMSIZ: usize = 15;
    assert!(format!("brhs{u}").len() <= IFNAMSIZ);
    let cleanup = Cleanup::new(&u);

    // Minimal CHV-owned boundary: bridge + veth member + guest namespace.
    sh(&["ip", "link", "add", &format!("brhs{u}"), "type", "bridge"]);
    sh(&["ip", "link", "set", &format!("brhs{u}"), "up"]);
    sh(&[
        "ip",
        "link",
        "add",
        &format!("gh{u}"),
        "type",
        "veth",
        "peer",
        "name",
        &format!("gg{u}"),
    ]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("gh{u}"),
        "master",
        &format!("brhs{u}"),
    ]);
    sh(&["ip", "link", "set", &format!("gh{u}"), "up"]);
    sh(&["ip", "netns", "add", &format!("gs{u}")]);
    sh(&[
        "ip",
        "link",
        "set",
        &format!("gg{u}"),
        "netns",
        &format!("gs{u}"),
    ]);

    let network_id = format!("hsx{u}");
    let table = format!("chv-{network_id}");
    let executor = LinuxExecutor::new(PathBuf::new());

    // Expose TCP 18080 -> 10.201.0.2:80.
    executor
        .expose_service(&network_id, "exp1", "tcp", 18080, "10.201.0.2", 80, "")
        .await
        .unwrap();
    assert!(
        chain_dump(&table, "forward").contains("exp1"),
        "exposure forward-accept rule must exist right after expose_service"
    );

    // Apply an EMPTY firewall policy via the same path the handler uses; this
    // rebuilds the forward base chain and re-asserts the stored exposure.
    executor
        .set_firewall_policy(&network_id, "v1", b"[]", &format!("brhs{u}"))
        .await
        .unwrap();
    let fwd_after = chain_dump(&table, "forward");
    assert!(
        fwd_after.contains("exp1"),
        "exposure forward-accept rule must be re-asserted after a firewall \
         apply (rebuild of the forward base chain must not drop exposed flows)"
    );
    assert!(
        fwd_after.contains("jump chv-policy-fwd"),
        "firewall guarded dispatch into default-deny must still be present"
    );

    // Withdraw -> exposure rules removed everywhere.
    executor
        .withdraw_service_exposure(&network_id, "exp1")
        .await
        .unwrap();
    assert!(
        !chain_dump(&table, "forward").contains("exp1"),
        "exposure forward-accept rule must be removed on withdraw"
    );
    assert!(
        !chain_dump(&table, "prerouting").contains("exp1"),
        "exposure prerouting DNAT must be removed on withdraw"
    );

    // Drop the executor-owned nft table (Cleanup only knows the chvhs-{u}
    // table; this one is created by LinuxExecutor as chv-{network_id}).
    let _ = Command::new("nft")
        .args(["delete", "table", "inet", &table])
        .output();

    drop(cleanup);
}

/// Privileged proof that the durable exposure journal is actually REPLAYED on
/// restart: a second executor (fresh in-memory state, same journal directory)
/// must re-install the DNAT/forward rules that the first executor applied.
///
/// This covers the install arm of `recover_exposures`; the skip arm is covered
/// unprivileged by `replay_skips_exposure_without_recovered_topology`.
///
/// Requires root + `nft`; skipped on CI. Run against the built test binary
/// under sudo with `--ignored --exact replay_reinstalls_journaled_exposure`.
#[tokio::test]
#[ignore = "requires root + real nftables on a Linux host; run locally via the fixture doc example"]
async fn replay_reinstalls_journaled_exposure() {
    use chv_nwd_core::executor::{LinuxExecutor, NetworkExecutor};
    use chv_nwd_core::exposure_store::ExposureStore;
    use std::path::PathBuf;

    if !is_root() || !nft_available() {
        tracing::info!("SKIP: replay_reinstalls_journaled_exposure needs root + nft");
        return;
    }

    /// Drop the executor-owned `chv-<net>` table even when an assertion fails.
    struct TableGuard(String);
    impl Drop for TableGuard {
        fn drop(&mut self) {
            let _ = Command::new("nft")
                .args(["delete", "table", "inet", &self.0])
                .output();
        }
    }

    let u = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:04x}{:04x}", std::process::id() & 0xffff, nanos & 0xffff)
    };
    let network_id = format!("hsr{u}");
    let table = format!("chv-{network_id}");
    let _guard = TableGuard(table.clone());
    let port = 30000 + (std::process::id() % 10000) as u32;

    let journal_dir = tempfile::tempdir().unwrap();
    let store = ExposureStore::new(journal_dir.path().join("exposures"));

    // First process lifetime: expose writes nft rules and the journal entry.
    let first = LinuxExecutor::new(PathBuf::new()).with_exposure_store(store.clone());
    first
        .expose_service(&network_id, "exp1", "tcp", port, "10.201.0.2", 80, "")
        .await
        .unwrap();
    assert!(
        store.path_for(&network_id).exists(),
        "expose_service must journal the applied exposure"
    );
    assert!(
        chain_dump(&table, "prerouting").contains(&port.to_string()),
        "the DNAT rule must exist after the first executor's expose"
    );

    // Simulate a restart: drop the table (kernel state is gone) but keep the
    // journal, then build a SECOND executor with the same store.
    let _ = Command::new("nft")
        .args(["delete", "table", "inet", &table])
        .output();
    assert!(
        !Command::new("nft")
            .args(["list", "table", "inet", &table])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
        "the table must be gone before replay so the assertion exercises reinstall"
    );

    let second = LinuxExecutor::new(PathBuf::new()).with_exposure_store(store.clone());
    let recoverable = std::collections::HashSet::from([network_id.clone()]);
    second.recover_exposures(&recoverable).await.unwrap();

    let table_dump = Command::new("nft")
        .args(["list", "table", "inet", &table])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        table_dump.contains(&port.to_string()),
        "replay must re-install the journaled DNAT rule: {table_dump}"
    );
    assert!(
        table_dump.contains("dnat"),
        "replayed rules must contain a DNAT expression: {table_dump}"
    );
    assert!(
        table_dump.contains("10.201.0.2"),
        "replayed DNAT must target the journaled IP: {table_dump}"
    );
    assert!(
        table_dump.contains("exp1"),
        "replayed rules must carry the journaled exposure comment marker: {table_dump}"
    );
}

/// Privileged regression for the durable exposure journal: expose writes it,
/// withdraw removes it, and a network delete (the derived-names teardown used
/// after a restart lost the state row) clears it. This is the path that used to
/// resurrect a deleted network's DNAT on the next nwd restart.
///
/// Requires root + `nft`; skipped on CI. Run against the built test binary
/// under sudo with `--ignored --exact network_delete_clears_exposure_journal`.
#[tokio::test]
#[ignore = "requires root + real nftables on a Linux host; run locally via the fixture doc example"]
async fn network_delete_clears_exposure_journal() {
    use chv_nwd_core::executor::{LinuxExecutor, NetworkExecutor};
    use chv_nwd_core::exposure_store::ExposureStore;
    use std::path::PathBuf;

    if !is_root() || !nft_available() {
        tracing::info!("SKIP: network_delete_clears_exposure_journal needs root + nft");
        return;
    }

    /// Drop the executor-owned `chv-<net>` table even when an assertion fails.
    struct TableGuard(String);
    impl Drop for TableGuard {
        fn drop(&mut self) {
            let _ = Command::new("nft")
                .args(["delete", "table", "inet", &self.0])
                .output();
        }
    }

    let u = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:04x}{:04x}", std::process::id() & 0xffff, nanos & 0xffff)
    };
    let network_id = format!("hsj{u}");
    let table = format!("chv-{network_id}");
    let _guard = TableGuard(table.clone());
    let journal_dir = tempfile::tempdir().unwrap();
    let store = ExposureStore::new(journal_dir.path().join("exposures"));
    let executor = LinuxExecutor::new(PathBuf::new()).with_exposure_store(store.clone());

    // expose -> journal entry written after the nft install succeeded.
    executor
        .expose_service(&network_id, "exp1", "tcp", 18081, "10.201.0.9", 80, "")
        .await
        .unwrap();
    assert!(
        store.path_for(&network_id).exists(),
        "expose_service must journal the applied exposure"
    );

    // withdraw -> last exposure removes the journal entry.
    executor
        .withdraw_service_exposure(&network_id, "exp1")
        .await
        .unwrap();
    assert!(
        !store.path_for(&network_id).exists(),
        "withdrawing the last exposure must remove the journal entry"
    );

    // Re-expose, then delete through the derived-names teardown (no state row).
    executor
        .expose_service(&network_id, "exp1", "tcp", 18081, "10.201.0.9", 80, "")
        .await
        .unwrap();
    assert!(store.path_for(&network_id).exists());

    executor
        .delete_local_topology_by_derived_names(&network_id)
        .await;
    assert!(
        !store.path_for(&network_id).exists(),
        "network delete must clear the durable exposure journal (otherwise \
         the next restart resurrects the deleted network's DNAT)"
    );

    // The teardown also drops the executor-owned table; be defensive in case
    // an earlier assertion failed before that ran.
    let _ = Command::new("nft")
        .args(["delete", "table", "inet", &table])
        .output();
}
