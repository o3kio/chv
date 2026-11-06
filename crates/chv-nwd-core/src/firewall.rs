use chv_errors::ChvError;
use serde::Deserialize;
use tokio::process::Command;
use tracing::info;

#[derive(Clone, Debug, Deserialize)]
pub struct FirewallRule {
    pub direction: String,
    pub protocol: String,
    pub source_cidr: Option<String>,
    pub dest_port: Option<String>,
    pub action: String,
}

const ALLOWED_PROTOCOLS: &[&str] = chv_common::firewall::PROTOCOLS;
const ALLOWED_ACTIONS: &[&str] = chv_common::firewall::ACTIONS;
const ALLOWED_DIRECTIONS: &[&str] = chv_common::firewall::DIRECTIONS;

fn validate_rule(rule: &FirewallRule) -> Result<(), ChvError> {
    if !ALLOWED_DIRECTIONS.contains(&rule.direction.as_str()) {
        return Err(ChvError::InvalidArgument {
            field: "direction".to_string(),
            reason: format!(
                "invalid direction '{}': must be inbound or outbound",
                rule.direction
            ),
        });
    }
    if !ALLOWED_PROTOCOLS.contains(&rule.protocol.as_str()) {
        return Err(ChvError::InvalidArgument {
            field: "protocol".to_string(),
            reason: format!(
                "invalid protocol '{}': must be one of tcp, udp, icmp, sctp, all",
                rule.protocol
            ),
        });
    }
    if !ALLOWED_ACTIONS.contains(&rule.action.as_str()) {
        return Err(ChvError::InvalidArgument {
            field: "action".to_string(),
            reason: format!(
                "invalid action '{}': must be accept, drop, or reject",
                rule.action
            ),
        });
    }
    if let Some(ref cidr) = rule.source_cidr {
        if !is_valid_cidr(cidr) {
            return Err(ChvError::InvalidArgument {
                field: "source_cidr".to_string(),
                reason: format!("invalid CIDR: '{}'", cidr),
            });
        }
    }
    if let Some(ref port) = rule.dest_port {
        if !is_valid_port_spec(port) {
            return Err(ChvError::InvalidArgument {
                field: "dest_port".to_string(),
                reason: format!("invalid port spec: '{}'", port),
            });
        }
        // Deliberately NOT rejected for icmp/all: rc.7's BFF accepted such a
        // rule and nwd applied it with the port ignored, so rejecting it here
        // would regress an upgrade (the stored ruleset would fail every
        // dispatch). The BFF gate rejects it for new saves; the renderer logs
        // a warning for legacy data instead of silently broadening it.
    }
    Ok(())
}

fn is_valid_cidr(cidr: &str) -> bool {
    chv_common::firewall::is_valid_cidr(cidr)
}

fn is_valid_port_spec(port: &str) -> bool {
    chv_common::firewall::is_valid_port_spec(port)
}

/// Policy chains (plain, non-hook) that carry CHV default-deny semantics inside
/// the CHV traffic boundary. Traffic that does not match the CHV-owned interface
/// guards below never reaches these chains.
const POLICY_FILTER_CHAINS: [&str; 3] = ["chv-policy-in", "chv-policy-fwd", "chv-policy-out"];

/// Build nftables argv for an interface guard (`iifname`/`oifname`) matching
/// exactly the given CHV-owned interface set.
///
/// The guard is the authoritative scope gate: CHV base-hook chains stay
/// `policy accept` for the host, and only CHV-owned guest traffic (topology
/// bridge plus enslaved TAP/veth members) is dispatched into the default-deny
/// policy chains. Returns an error when the owned set is empty so CHV never
/// guesses a host interface to scope policy against (fail closed).
fn iface_guard_args(kind: &str, owned_ifaces: &[String]) -> Result<Vec<String>, ChvError> {
    if owned_ifaces.is_empty() {
        return Err(ChvError::InvalidArgument {
            field: "owned_ifaces".to_string(),
            reason: "cannot scope CHV firewall policy: no CHV-owned interface is known \
                 (topology/interface ownership is authoritative; refusing to guess a host \
                 interface)"
                .to_string(),
        });
    }
    let mut args = vec![kind.to_string()];
    if owned_ifaces.len() == 1 {
        args.push(owned_ifaces[0].clone());
    } else {
        args.push("{".to_string());
        for (i, iface) in owned_ifaces.iter().enumerate() {
            let trailing = if i + 1 < owned_ifaces.len() { "," } else { "" };
            args.push(format!("\"{}\"{}", iface, trailing));
        }
        args.push("}".to_string());
    }
    Ok(args)
}

fn add_rule_prefix(table: &str, chain: &str) -> Vec<String> {
    ["add", "rule", "inet", table, chain]
        .into_iter()
        .map(String::from)
        .collect()
}

async fn run_nft_strings(args: Vec<String>) -> Result<(), ChvError> {
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_nft(&refs).await
}

/// A `(chain, rule)` plan in the exact order `apply_firewall_rules` must
/// INSERT each user rule at the chain head.
///
/// The plan sorts user rules deny/reject-first then accept (so a broad accept
/// cannot shadow a more specific deny within a chain) and maps each rule to its
/// target chains. The apply inserts at the head of each chain — so the terminal
/// `counter drop` installed earlier is never removed — and inserting this
/// sequence in reverse reproduces, per chain, the same oldest-to-newest final
/// ordering a plain `add`-based build would have produced (see the unit tests
/// for the invariant).
fn user_rule_insertion_plan(rules: &[FirewallRule]) -> Vec<(&'static str, &FirewallRule)> {
    // Deny/reject rules first, then accept rules, so a broad accept cannot
    // shadow a more specific deny within a chain. Inserting this sequence at
    // each chain head in REVERSE reproduces, per chain, the same
    // oldest-to-newest final ordering a plain `add`-based build would have
    // produced (see the unit tests for the invariant).
    rules
        .iter()
        .filter(|r| r.action == "drop" || r.action == "reject")
        .chain(rules.iter().filter(|r| r.action == "accept"))
        .rev()
        .flat_map(|rule| {
            // Cover the real traffic paths: host-stack (input/output) AND the
            // guest forwarding path (forward), which the original
            // input/output-only mapping missed (per #227).
            let chains: &[&str] = match rule.direction.as_str() {
                "inbound" => &["chv-policy-in", "chv-policy-fwd"],
                "outbound" => &["chv-policy-out", "chv-policy-fwd"],
                _ => &[],
            };
            chains
                .iter()
                .map(move |chain| (*chain, rule))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_firewall_rules(policy_json: &[u8]) -> Result<Vec<FirewallRule>, ChvError> {
    if policy_json.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(policy_json).map_err(|e| ChvError::InvalidArgument {
        field: "policy_json".to_string(),
        reason: format!("failed to parse firewall rules: {}", e),
    })
}

/// Validate a firewall payload without touching the host.
///
/// The RPC boundary calls this before journaling a policy as desired state.
/// A payload the engine can never apply must be rejected up front: journaling
/// it would turn the next documented restart into a permanent crash loop,
/// because recovery re-applies the journal fail-closed before serving.
pub fn validate_firewall_policy(policy_json: &[u8]) -> Result<(), ChvError> {
    for rule in &parse_firewall_rules(policy_json)? {
        validate_rule(rule)?;
    }
    Ok(())
}

pub async fn apply_firewall_rules(
    table: &str,
    owned_ifaces: &[String],
    policy_json: &[u8],
) -> Result<(), ChvError> {
    let rules = parse_firewall_rules(policy_json)?;

    for rule in &rules {
        validate_rule(rule)?;
    }

    // The ownership guard must resolve before any rule is written; this fails
    // closed (no guessed host interface) per the host-safety invariant.
    let iif_guard = iface_guard_args("iifname", owned_ifaces)?;
    let oif_guard = iface_guard_args("oifname", owned_ifaces)?;

    // Ensure the CHV table exists.
    run_nft_idempotent(&["add", "table", "inet", table]).await?;

    // Base hook chains: policy accept for the host. CHV policy is confined to
    // CHV-owned guest traffic via the guards below; unrelated host, container,
    // CNI, SSH and forwarded traffic is never evaluated by a CHV drop policy.
    // (Previously these chains used `policy drop` with no interface guards,
    // which dropped all host traffic — the bug tracked by #227.)
    //
    // The chains are deleted and re-created on every apply (not just added
    // idempotently) so a stale `policy drop` base chain left behind by a
    // previous daemon version is replaced with `policy accept` on upgrade;
    // `add chain` alone would silently keep the old drop policy (#227 S4).
    //
    // Transient disclosure: between deleting and re-adding each base chain
    // (sub-millisecond, single-writer under the executor nft lock) CHV-owned
    // guest traffic is not yet dispatched into default-deny — briefly
    // unguarded for CHV guests only. The host is NEVER affected: the base
    // policy is `accept` throughout, and no host interface matches the guards.
    for (chain, hook) in [
        ("input", "input"),
        ("forward", "forward"),
        ("output", "output"),
    ] {
        delete_chain_quiet(table, chain).await?;
        run_nft_idempotent(&[
            "add",
            "chain",
            "inet",
            table,
            chain,
            &format!(
                "{{ type filter hook {} priority filter ; policy accept ; }}",
                hook
            ),
        ])
        .await?;
    }

    // Verify the base hooks really are `policy accept`. If a delete was
    // transiently refused and the old `policy drop` chain survived the
    // idempotent add, fail closed instead of reproducing #227 silently.
    verify_base_chain_policies(table).await?;

    // Plain (non-hook) policy chains carry default-deny inside the boundary.
    // Created before any dispatch rule so a guard jump never targets a
    // missing chain.
    for pchain in POLICY_FILTER_CHAINS {
        run_nft_idempotent(&["add", "chain", "inet", table, pchain]).await?;
    }

    // Default-deny-FIRST: flush each policy chain and immediately re-install
    // its terminal `counter drop`, before any dispatch rule and before any user
    // rule exists. If the apply fails at ANY later point — notably between
    // dispatch installs on a fresh table where the policy chains were just
    // created empty — every policy chain still ends in drop, so CHV-owned guest
    // traffic remains default-denied (never fail-open).
    for pchain in POLICY_FILTER_CHAINS {
        if let Err(e) = run_nft(&["flush", "chain", "inet", table, pchain]).await {
            tracing::warn!(table, chain = pchain, error = %e, "failed to flush nftables policy chain");
        }
        run_nft(&["add", "rule", "inet", table, pchain, "counter", "drop"]).await?;
    }

    // Dispatch ONLY CHV-owned traffic into the policy chains.
    let mut input = add_rule_prefix(table, "input");
    input.extend(iif_guard.clone());
    input.extend(["jump".to_string(), "chv-policy-in".to_string()]);
    run_nft_strings(input).await?;

    let mut fwd_in = add_rule_prefix(table, "forward");
    fwd_in.extend(iif_guard.clone());
    fwd_in.extend(["jump".to_string(), "chv-policy-fwd".to_string()]);
    run_nft_strings(fwd_in).await?;

    let mut fwd_out = add_rule_prefix(table, "forward");
    fwd_out.extend(oif_guard.clone());
    fwd_out.extend(["jump".to_string(), "chv-policy-fwd".to_string()]);
    run_nft_strings(fwd_out).await?;

    let mut out = add_rule_prefix(table, "output");
    out.extend(oif_guard.clone());
    out.extend(["jump".to_string(), "chv-policy-out".to_string()]);
    run_nft_strings(out).await?;

    // Apply user rules via the insertion plan (deny/reject first, then accept,
    // so a broad accept cannot shadow a more specific deny). Rules are INSERTED
    // at each chain head, which reproduces the same oldest-to-newest final
    // ordering as `add` while never removing the default-deny terminal that was
    // installed above the dispatch rules.
    for (chain, rule) in user_rule_insertion_plan(&rules) {
        let mut args: Vec<&str> = vec!["insert", "rule", "inet", table, chain];

        // Protocol match (skip for "all")
        let protocol_lower = rule.protocol.to_lowercase();
        if protocol_lower != "all" {
            args.push("meta");
            args.push("l4proto");
            args.push(&protocol_lower);
        }

        // Source CIDR match
        let cidr_owned;
        if let Some(ref cidr) = rule.source_cidr {
            args.push("ip");
            args.push("saddr");
            cidr_owned = cidr.clone();
            args.push(&cidr_owned);
        }

        // Destination port match
        let port_owned;
        if let Some(ref port) = rule.dest_port {
            if matches!(protocol_lower.as_str(), "tcp" | "udp" | "sctp") {
                args.push(&protocol_lower);
                args.push("dport");
                port_owned = port.clone();
                args.push(&port_owned);
            } else {
                // Legacy ruleset from before the save gate tightened: keep the
                // old behavior (apply, ignore the port) instead of failing an
                // upgrade, but say loudly that the constraint is not enforced.
                tracing::warn!(
                    network_table = %table,
                    protocol = %rule.protocol,
                    dest_port = %port,
                    "dest_port is not valid for this protocol and is ignored; \
                     re-save the rule to remove the field"
                );
            }
        }

        // Action
        let action = rule.action.to_lowercase();
        args.push(&action);

        run_nft(&args).await?;
    }

    // Conntrack established/related is inserted LAST so it lands at the head
    // of each chain (before user rules), preserving established-flow semantics
    // across policy replaces: existing guest flows — including host->guest
    // replies on the output path — are not torn down by default-deny.
    for chain in ["chv-policy-in", "chv-policy-fwd", "chv-policy-out"] {
        run_nft(&[
            "insert",
            "rule",
            "inet",
            table,
            chain,
            "ct",
            "state",
            "established,related",
            "accept",
        ])
        .await?;
    }

    info!(
        table = %table,
        rule_count = rules.len(),
        "firewall rules applied (CHV-owned boundary)"
    );
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
pub struct NatRule {
    pub source_cidr: String,
    pub dest_cidr: Option<String>,
    pub masquerade: Option<bool>,
}

fn parse_nat_rules(policy_json: &[u8]) -> Result<Vec<NatRule>, ChvError> {
    if policy_json.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(policy_json).map_err(|e| ChvError::InvalidArgument {
        field: "policy_json".to_string(),
        reason: format!("failed to parse NAT rules: {}", e),
    })
}

fn validate_nat_rule(rule: &NatRule) -> Result<(), ChvError> {
    if !is_valid_cidr(&rule.source_cidr) {
        return Err(ChvError::InvalidArgument {
            field: "source_cidr".to_string(),
            reason: format!("invalid CIDR: '{}'", rule.source_cidr),
        });
    }
    if let Some(ref dest) = rule.dest_cidr {
        if !is_valid_cidr(dest) {
            return Err(ChvError::InvalidArgument {
                field: "dest_cidr".to_string(),
                reason: format!("invalid CIDR: '{}'", dest),
            });
        }
    }
    Ok(())
}

/// Validate a NAT payload without touching the host (see
/// [`validate_firewall_policy`]). NAT is not the isolation boundary, but an
/// unapplicable desired-state entry would still poison restart recovery.
pub fn validate_nat_policy(policy_json: &[u8]) -> Result<(), ChvError> {
    for rule in &parse_nat_rules(policy_json)? {
        validate_nat_rule(rule)?;
    }
    Ok(())
}

pub async fn apply_nat_rules(
    table: &str,
    owned_ifaces: &[String],
    policy_json: &[u8],
) -> Result<(), ChvError> {
    let rules = parse_nat_rules(policy_json)?;

    for rule in &rules {
        validate_nat_rule(rule)?;
    }

    let iif_guard = iface_guard_args("iifname", owned_ifaces)?;

    // Ensure table and postrouting chain exist.
    run_nft_idempotent(&["add", "table", "inet", table]).await?;
    run_nft_idempotent(&[
        "add",
        "chain",
        "inet",
        table,
        "postrouting",
        "{ type nat hook postrouting priority 100 ; policy accept ; }",
    ])
    .await?;

    // Flush existing NAT rules.
    if let Err(e) = run_nft(&["flush", "chain", "inet", table, "postrouting"]).await {
        tracing::warn!(table, error = %e, "failed to flush postrouting chain");
    }

    if rules.is_empty() {
        // Default masquerade ONLY for CHV-owned guest traffic leaving the host.
        // (Previously any non-loopback traffic on the host was masqueraded.)
        let mut args = add_rule_prefix(table, "postrouting");
        args.extend(iif_guard);
        args.extend([
            "oif".to_string(),
            "!=".to_string(),
            "lo".to_string(),
            "masquerade".to_string(),
        ]);
        run_nft_strings(args).await?;
    } else {
        for rule in &rules {
            let mut args = add_rule_prefix(table, "postrouting");
            args.extend(iif_guard.clone());

            args.push("ip".to_string());
            args.push("saddr".to_string());
            args.push(rule.source_cidr.clone());

            if let Some(ref dest) = rule.dest_cidr {
                args.push("ip".to_string());
                args.push("daddr".to_string());
                args.push(dest.clone());
            }

            if rule.masquerade.unwrap_or(true) {
                args.push("masquerade".to_string());
            }

            run_nft_strings(args).await?;
        }
    }

    info!(
        table = %table,
        rule_count = rules.len(),
        "NAT rules applied (CHV-owned boundary)"
    );
    Ok(())
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

async fn run_nft_idempotent(args: &[&str]) -> Result<(), ChvError> {
    match run_nft(args).await {
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

/// Delete a chain if it exists, tolerating a missing chain; any OTHER failure
/// (e.g. a transient netlink error) is propagated so the apply aborts and the
/// caller sees the error instead of silently keeping a stale chain policy.
///
/// Used to replace stale base-hook chain policies (e.g. the pre-#227
/// `policy drop`) with `policy accept` on every apply, so an in-place upgrade
/// of a running daemon cannot leave a host-wide drop base chain active.
async fn delete_chain_quiet(table: &str, chain: &str) -> Result<(), ChvError> {
    match run_nft(&["delete", "chain", "inet", table, chain]).await {
        Ok(()) => Ok(()),
        Err(ChvError::NetworkUnavailable { reason, .. }) => {
            if reason.contains("No such file or directory") || reason.contains("does not exist") {
                Ok(())
            } else {
                Err(ChvError::NetworkUnavailable {
                    resource: "nft".to_string(),
                    reason: format!(
                        "failed to delete nft chain {chain} before re-adding with accept policy: {reason}"
                    ),
                })
            }
        }
        Err(e) => Err(e),
    }
}

/// Post-apply verification that every base hook chain really is `policy accept`.
///
/// This is the fail-closed net for upgrade safety: if `delete_chain_quiet`
/// hit a transient refusal, the stale pre-#227 `policy drop` chain would
/// otherwise survive the idempotent `add chain` (`File exists` -> Ok) and
/// reproduce the host-wide-drop bug without any error. Each hook name maps to
/// exactly one chain in the CHV-owned table, so the `type filter hook X
/// priority filter; policy accept;` substring is unique to the base chain.
async fn verify_base_chain_policies(table: &str) -> Result<(), ChvError> {
    let out = Command::new("nft")
        .args(["list", "table", "inet", table])
        .output()
        .await
        .map_err(|e| ChvError::Io {
            path: "nft".to_string(),
            source: e,
        })?;
    if !out.status.success() {
        return Err(ChvError::NetworkUnavailable {
            resource: "nft".to_string(),
            reason: format!("nft list table inet {table} failed during post-apply verification"),
        });
    }
    let dump = String::from_utf8_lossy(&out.stdout).into_owned();
    for (chain, hook) in [
        ("input", "input"),
        ("forward", "forward"),
        ("output", "output"),
    ] {
        let needle = format!("type filter hook {hook} priority filter; policy accept;");
        if !dump.contains(&needle) {
            return Err(ChvError::NetworkUnavailable {
                resource: "nft".to_string(),
                reason: format!(
                    "post-apply verification failed: base chain {chain} is not \
                     `policy accept` (stale policy-drop base chain would break #227, \
                     refusing to proceed)"
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_rule_valid() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: Some("10.0.0.0/24".to_string()),
            dest_port: Some("443".to_string()),
            action: "accept".to_string(),
        };
        assert!(validate_rule(&rule).is_ok());
    }

    #[test]
    fn test_validate_rule_invalid_direction() {
        let rule = FirewallRule {
            direction: "sideways".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: None,
            dest_port: None,
            action: "accept".to_string(),
        };
        assert!(validate_rule(&rule).is_err());
    }

    #[test]
    fn test_validate_rule_invalid_protocol() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "gopher".to_string(),
            source_cidr: None,
            dest_port: None,
            action: "accept".to_string(),
        };
        assert!(validate_rule(&rule).is_err());
    }

    #[test]
    fn test_validate_rule_invalid_action() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: None,
            dest_port: None,
            action: "explode".to_string(),
        };
        assert!(validate_rule(&rule).is_err());
    }

    #[test]
    fn test_validate_rule_invalid_cidr() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: Some("not-a-cidr".to_string()),
            dest_port: None,
            action: "accept".to_string(),
        };
        assert!(validate_rule(&rule).is_err());
    }

    #[test]
    fn test_validate_rule_port_range() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: None,
            dest_port: Some("8000-9000".to_string()),
            action: "drop".to_string(),
        };
        assert!(validate_rule(&rule).is_ok());
    }

    #[test]
    fn test_validate_rule_invalid_port() {
        let rule = FirewallRule {
            direction: "inbound".to_string(),
            protocol: "tcp".to_string(),
            source_cidr: None,
            dest_port: Some("abc".to_string()),
            action: "accept".to_string(),
        };
        assert!(validate_rule(&rule).is_err());
    }

    #[test]
    fn test_is_valid_cidr() {
        assert!(is_valid_cidr("10.0.0.0/24"));
        assert!(is_valid_cidr("192.168.1.0/16"));
        // The engine renders IPv4 `ip saddr` only (see chv_common::firewall).
        assert!(!is_valid_cidr("::1/128"));
        assert!(!is_valid_cidr("fd00::/8"));
        assert!(!is_valid_cidr("10.0.0.0"));
        assert!(!is_valid_cidr("not-ip/24"));
        assert!(!is_valid_cidr("10.0.0.0/999"));
    }

    #[test]
    fn test_parse_empty_policy() {
        let rules: Vec<FirewallRule> = serde_json::from_slice(b"[]").unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_parse_policy_json() {
        let json = r#"[
            {"direction": "inbound", "protocol": "tcp", "source_cidr": "10.0.0.0/8", "dest_port": "22", "action": "accept"},
            {"direction": "outbound", "protocol": "all", "action": "accept"}
        ]"#;
        let rules: Vec<FirewallRule> = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].dest_port.as_deref(), Some("22"));
        assert!(rules[1].source_cidr.is_none());
    }

    #[test]
    fn test_iface_guard_args_empty_fails_closed() {
        assert!(iface_guard_args("iifname", &[]).is_err());
        assert!(iface_guard_args("oifname", &[]).is_err());
    }

    #[test]
    fn test_iface_guard_args_single() {
        let args = iface_guard_args("iifname", &["br-net1".to_string()]).unwrap();
        assert_eq!(args, vec!["iifname", "br-net1"]);
    }

    #[test]
    fn test_iface_guard_args_multiple() {
        let args =
            iface_guard_args("oifname", &["br-net1".to_string(), "tap-1111".to_string()]).unwrap();
        assert_eq!(
            args,
            vec!["oifname", "{", "\"br-net1\",", "\"tap-1111\"", "}"]
        );
    }

    fn mk_rule<'a>(direction: &'a str, action: &'a str, protocol: &'a str) -> FirewallRule {
        FirewallRule {
            direction: direction.to_string(),
            protocol: protocol.to_string(),
            source_cidr: None,
            dest_port: None,
            action: action.to_string(),
        }
    }

    #[test]
    fn user_rule_plan_final_order_is_deny_before_accept_per_chain() {
        let rules = vec![
            mk_rule("inbound", "drop", "tcp"),
            mk_rule("outbound", "reject", "udp"),
            mk_rule("inbound", "accept", "icmp"),
            mk_rule("outbound", "accept", "tcp"),
        ];
        let plan = user_rule_insertion_plan(&rules);

        // `insert` prepends, so the final per-chain order is the REVERSE of the
        // plan's per-chain insertion sequence.
        let mut final_by_chain: std::collections::HashMap<&'static str, Vec<&FirewallRule>> =
            std::collections::HashMap::new();
        for (chain, rule) in plan.iter().rev() {
            final_by_chain.entry(*chain).or_default().push(*rule);
        }

        // Every policy chain is populated; forward gets inbound+outbound.
        assert_eq!(final_by_chain.get("chv-policy-in").map(Vec::len), Some(2));
        assert_eq!(final_by_chain.get("chv-policy-out").map(Vec::len), Some(2));
        assert_eq!(final_by_chain.get("chv-policy-fwd").map(Vec::len), Some(4));

        for (chain, final_rules) in &final_by_chain {
            for r in final_rules.iter() {
                // Membership: inbound rules never target chv-policy-out and
                // outbound rules never target chv-policy-in.
                let illegal = matches!(
                    (r.direction.as_str(), *chain),
                    ("inbound", "chv-policy-out") | ("outbound", "chv-policy-in")
                );
                assert!(!illegal, "rule {} must not target {}", r.direction, chain);
            }
            // Deny/reject rules must all precede accept rules within a chain.
            let first_accept = final_rules.iter().position(|r| r.action == "accept");
            if let Some(idx) = first_accept {
                assert!(
                    final_rules[idx..].iter().all(|r| r.action == "accept"),
                    "an accept rule appears before a deny/reject rule in {chain}"
                );
            }
        }
    }

    #[test]
    fn validate_policy_rejects_unapplicable_payloads_without_touching_the_host() {
        // The RPC journals desired state before applying it. A payload the
        // engine can never apply must be rejected up front: journaling it
        // would turn the next documented restart into a permanent crash loop.
        assert!(validate_firewall_policy(b"").is_ok());
        assert!(validate_firewall_policy(b"[]").is_ok());
        assert!(validate_firewall_policy(
            br#"[{"direction":"inbound","protocol":"tcp","action":"accept","dest_port":"443"}]"#
        )
        .is_ok());

        // Malformed JSON, invalid direction, and a malformed CIDR all fail.
        assert!(matches!(
            validate_firewall_policy(b"{"),
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_firewall_policy(
                br#"[{"direction":"sideways","protocol":"tcp","action":"accept"}]"#
            ),
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_firewall_policy(
                br#"[{"direction":"inbound","protocol":"tcp","action":"accept","source_cidr":"10.0.0.1"}]"#
            ),
            Err(ChvError::InvalidArgument { .. })
        ));
        // IPv6 is rejected: the engine renders `ip saddr` only, so accepting
        // it here would journal a payload that can never apply.
        assert!(matches!(
            validate_firewall_policy(
                br#"[{"direction":"inbound","protocol":"tcp","action":"accept","source_cidr":"fd00::/8"}]"#
            ),
            Err(ChvError::InvalidArgument { .. })
        ));
        // Lexical forms Rust's parsers accept but nft rejects must not reach
        // the journal either.
        assert!(matches!(
            validate_firewall_policy(
                br#"[{"direction":"inbound","protocol":"tcp","action":"accept","source_cidr":"10.0.0.0/+24"}]"#
            ),
            Err(ChvError::InvalidArgument { .. })
        ));
        // nft reads a leading zero as octal; `/030` means /24, so a
        // non-canonical prefix must not reach the journal.
        assert!(matches!(
            validate_firewall_policy(
                br#"[{"direction":"inbound","protocol":"tcp","action":"accept","source_cidr":"10.0.0.0/030"}]"#
            ),
            Err(ChvError::InvalidArgument { .. })
        ));
        for port in [
            "8090-8080",
            "+443",
            "+8080-+8090",
            "0443",
            "0700",
            "600-0777",
        ] {
            let payload = format!(
                r#"[{{"direction":"inbound","protocol":"tcp","action":"accept","dest_port":"{port}"}}]"#
            );
            assert!(
                matches!(
                    validate_firewall_policy(payload.as_bytes()),
                    Err(ChvError::InvalidArgument { .. })
                ),
                "port spec {port} would be rejected or reinterpreted by nft and must not be accepted"
            );
        }
        // Legacy rc.7 rulesets may carry a dest_port on icmp/all. nwd keeps
        // applying them (the port is ignored, with a warning) so an upgrade
        // does not regress; the BFF gate rejects the combination for new saves.
        assert!(validate_firewall_policy(
            br#"[{"direction":"inbound","protocol":"icmp","action":"accept","dest_port":"443"}]"#
        )
        .is_ok());
        assert!(validate_firewall_policy(
            br#"[{"direction":"inbound","protocol":"all","action":"accept","dest_port":"443"}]"#
        )
        .is_ok());
        // sctp does carry ports and the renderer emits `sctp dport`.
        assert!(validate_firewall_policy(
            br#"[{"direction":"inbound","protocol":"sctp","action":"accept","dest_port":"8080-8090"}]"#
        )
        .is_ok());

        // NAT: same contract (BFF stores nat_rules verbatim, so this is the
        // only validation on the path).
        assert!(validate_nat_policy(b"").is_ok());
        assert!(validate_nat_policy(br#"[{"source_cidr":"10.0.0.0/24"}]"#).is_ok());
        assert!(matches!(
            validate_nat_policy(br#"[{"source_cidr":"10.0.0.1"}]"#),
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_nat_policy(br#"[{"source_cidr":"fd00::/8"}]"#),
            Err(ChvError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_nat_policy(b"not json"),
            Err(ChvError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn user_rule_plan_mixed_actions_keep_relative_deny_order() {
        let rules = vec![
            mk_rule("inbound", "drop", "tcp"),    // deny-1
            mk_rule("inbound", "drop", "udp"),    // deny-2
            mk_rule("inbound", "accept", "icmp"), // accept-1
            mk_rule("outbound", "drop", "all"),   // deny-3
        ];
        let plan = user_rule_insertion_plan(&rules);
        let mut final_by_chain: std::collections::HashMap<&'static str, Vec<&FirewallRule>> =
            std::collections::HashMap::new();
        for (chain, rule) in plan.iter().rev() {
            final_by_chain.entry(*chain).or_default().push(*rule);
        }

        let fwd = final_by_chain.get("chv-policy-fwd").unwrap();
        // deny-1, deny-2, deny-3, accept-1 (denies keep their relative order).
        assert_eq!(
            fwd.iter().map(|r| r.protocol.as_str()).collect::<Vec<_>>(),
            vec!["tcp", "udp", "all", "icmp",]
        );
        let den: Vec<&str> = fwd.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(den, vec!["drop", "drop", "drop", "accept"]);
    }
}
