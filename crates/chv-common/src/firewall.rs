//! The network firewall-rule vocabulary (#355/#368): ONE definition shared
//! by every layer that speaks rules — the BFF's save-time validation and
//! nwd's enforcement engine.
//!
//! Why this module exists: the UI's firewall editor and the (undispatched)
//! `/v1/firewall-rules` CRUD invented a parallel dialect
//! (`ingress`/`egress`, `allow`/`deny`, `port_range`), while nwd's engine
//! has always required `inbound`/`outbound`, `accept`/`drop`/`reject`,
//! `dest_port`. Pre-#355 nothing dispatched network policy to nodes, so
//! the split was invisible. Once #355 rode the persisted ruleset on the
//! VM spec, a dialect rule detonated at attach time (`attach_vm_nic`'s
//! policy-guard refresh rejected it) and terminally failed the VM create
//! with an opaque RUNTIME_UNAVAILABLE — the M4.4 re-qualification's N7.
//!
//! The engine vocabulary is the contract. Producers validate at save time
//! (fail fast, where the operator sees it); nwd stays strict (defense in
//! depth: a ruleset that somehow bypasses the save gate still fails loudly
//! at the enforcement point, never silently).

/// Rule directions nwd's engine accepts. The UI-era aliases
/// `ingress`/`egress` are NOT valid.
pub const DIRECTIONS: &[&str] = &["inbound", "outbound"];

/// Rule protocols nwd's engine accepts.
pub const PROTOCOLS: &[&str] = &["tcp", "udp", "icmp", "sctp", "all"];

/// Rule actions nwd's engine accepts. The UI-era aliases `allow`/`deny`
/// are NOT valid.
pub const ACTIONS: &[&str] = &["accept", "drop", "reject"];

/// The exact set of keys a rule object may carry (nwd's `FirewallRule`
/// fields). Anything else — `source`, `port_range`, `priority`,
/// `description` — is rejected at save time so a silently-dropped field
/// can never narrow a rule the operator meant to be scoped.
pub const RULE_KEYS: &[&str] = &[
    "direction",
    "protocol",
    "action",
    "source_cidr",
    "dest_port",
];

/// Is `cidr` a syntactically valid **IPv4** CIDR block (nwd's engine
/// semantics, shared with the BFF's save-time gate)?
///
/// The engine renders every rule as `ip saddr` / `ip daddr`, so an IPv6 block
/// would be accepted by a generic validator and then fail deterministically at
/// apply time. That divergence is not harmless: the policy journal records the
/// desired payload and replay is fail-closed, so an IPv6 rule would poison the
/// next nwd restart. Reject it at the gate until the engine renders `ip6`.
pub fn is_valid_cidr(cidr: &str) -> bool {
    let parts: Vec<&str> = cidr.splitn(2, '/').collect();
    if parts.len() != 2 {
        return false;
    }
    match parts[0].parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => parts[1].parse::<u8>().map(|p| p <= 32).unwrap_or(false),
        _ => false,
    }
}

/// Is `port` a valid single port or port range (`443`, `8080-8090`)?
pub fn is_valid_port_spec(port: &str) -> bool {
    if port.contains('-') {
        let parts: Vec<&str> = port.splitn(2, '-').collect();
        parts.len() == 2 && parts[0].parse::<u16>().is_ok() && parts[1].parse::<u16>().is_ok()
    } else {
        port.parse::<u16>().is_ok()
    }
}

/// The #355 DP4 baseline ruleset as raw data: `(direction, protocol,
/// action, dest_port)` tuples, validated against the engine vocabulary
/// by the test below. Rendering to policy JSON happens in
/// [`baseline_policy_json`] (this crate is deliberately serde_json-free).
const BASELINE_RULES: &[(&str, &str, &str, &str)] = &[
    // DHCP requests (guest → host dnsmasq, input path).
    ("inbound", "udp", "accept", "67-68"),
    // DHCP replies (host → guest, output path) — belt-and-suspenders
    // alongside conntrack: the initial `0.0.0.0:68 →
    // 255.255.255.255:67` broadcast is exactly the flow class UDP
    // conntrack tracks least reliably.
    ("outbound", "udp", "accept", "67-68"),
    // DNS queries (guest → host resolver, input path). The replies
    // ride the engine's established/related conntrack rule (inserted
    // at every chain head by `apply_firewall_rules`) — no baseline
    // rule can or should cover the ephemeral reply ports.
    ("inbound", "udp", "accept", "53"),
    ("inbound", "tcp", "accept", "53"),
];

/// The #355 DP4 baseline ruleset as policy JSON: what a network with
/// NO user rules gets — `[]` and never-set both mean "no user rules",
/// and the boundary is **baseline + default-deny** (ruled 2026-10-08),
/// never a bare default-deny (the #360 cutoff, which cut a rule-less
/// network's guests off entirely, including DHCP) and never unfiltered
/// (the pre-DP4 skip). Everything inside the CHV traffic boundary that
/// the baseline does not allow stays default-deny.
///
/// Scope note (as-landed delta from the design's "DHCP/DNS to the
/// gateway"): the rule vocabulary has no destination CIDR (`RULE_KEYS`
/// is direction/protocol/action/source_cidr/dest_port) and §7 rules
/// the nwd engine unchanged, so the allows are port-scoped on
/// CHV-owned interfaces — a guest may also reach a non-gateway DNS
/// resolver on port 53. The interface guards already scope the policy
/// to this topology's guests; default-deny still covers every other
/// port and protocol.
pub fn baseline_policy_json() -> String {
    let mut out = String::with_capacity(160);
    out.push('[');
    for (i, (direction, protocol, action, dest_port)) in BASELINE_RULES.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"direction":"{direction}","protocol":"{protocol}","action":"{action}","dest_port":"{dest_port}"}}"#
        ));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_validator_accepts_v4_and_rejects_v6_blocks() {
        assert!(is_valid_cidr("10.200.0.0/24"));
        assert!(is_valid_cidr("0.0.0.0/0"));
        // The engine renders `ip saddr` only; accepting v6 here would journal
        // a payload that can never apply (fail-closed recovery crash loop).
        assert!(!is_valid_cidr("fd00::/8"));
        assert!(!is_valid_cidr("::1/128"));
        assert!(!is_valid_cidr("10.200.0.0")); // no prefix
        assert!(!is_valid_cidr("10.200.0.0/33")); // bad prefix
        assert!(!is_valid_cidr("not-an-ip/24"));
    }

    #[test]
    fn port_spec_validator_accepts_ports_and_ranges() {
        assert!(is_valid_port_spec("443"));
        assert!(is_valid_port_spec("8080-8090"));
        assert!(!is_valid_port_spec("80-")); // dangling range
        assert!(!is_valid_port_spec("ssh")); // no service names
        assert!(!is_valid_port_spec("0-99999")); // out of u16
    }

    #[test]
    fn baseline_ruleset_is_engine_valid_vocabulary() {
        // The DP4 baseline is hand-authored data, so a vocabulary drift
        // (a renamed key, a new required field) would detonate only at
        // attach time on a rule-less network — the exact M4.4 N7 class.
        // Pin it here: every baseline rule must satisfy the same gate
        // the BFF applies to operator rules.
        assert!(
            !BASELINE_RULES.is_empty(),
            "the baseline must not be empty (that is the #360 cutoff)"
        );
        for (direction, protocol, action, dest_port) in BASELINE_RULES {
            assert!(
                DIRECTIONS.contains(direction),
                "baseline direction '{direction}'"
            );
            assert!(
                PROTOCOLS.contains(protocol),
                "baseline protocol '{protocol}'"
            );
            assert!(ACTIONS.contains(action), "baseline action '{action}'");
            assert!(
                is_valid_port_spec(dest_port),
                "baseline dest_port '{dest_port}'"
            );
        }
        // The rendered JSON is a semantically non-empty ruleset, so the
        // DP4 dispatch/attach paths never feed it back into the empty
        // branch, and every rule carries exactly the four keys above.
        let json = baseline_policy_json();
        assert!(!crate::firewall_ruleset_is_empty(&json));
        assert_eq!(
            json.matches("direction").count(),
            BASELINE_RULES.len(),
            "one direction key per rule: {json}"
        );
        for forbidden in ["ingress", "egress", "allow", "deny\"", "priority"] {
            assert!(
                !json.contains(forbidden),
                "the baseline must not carry UI-dialect tokens ('{forbidden}'): {json}"
            );
        }
        assert!(
            json.starts_with('[') && json.ends_with(']'),
            "the baseline must be a JSON array: {json}"
        );
    }
}
