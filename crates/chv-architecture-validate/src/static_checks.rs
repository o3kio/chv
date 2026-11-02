//! Layer-1 static checks for `CHVArchitecture` documents.
//!
//! Every check produces zero or more [`Finding`]s with the stable `code`
//! values declared in [`crate::codes`]. The checks here cover the static
//! validation layer of `architecture-designer-validation.md` plus the
//! contract-level rules that are not in the suggested code list (user
//! namespace separation, static-IP-in-DHCP-range warning).
//!
//! # Determinism
//!
//! Pairwise checks (CIDR overlap, IP collision) iterate in input order so
//! the emitted findings are stable across runs. UI tests pin against the
//! resulting finding set.

use chv_controlplane_types::architecture::{Finding, Severity};
use ipnetwork::IpNetwork;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use crate::codes::{
    ALLOWED_PERMISSIONS, DHCP_RANGE_INVALID, DUPLICATE_IP, DUPLICATE_NAME, GATEWAY_OUTSIDE_NETWORK,
    INVALID_CIDR, INVALID_EDGE, INVALID_PERMISSION, IP_OUTSIDE_NETWORK, MISSING_REFERENCE,
    NETWORK_CIDR_OVERLAP, RAW_SECRET_FORBIDDEN, STATIC_IP_IN_DHCP_RANGE, USER_NAMESPACE_COLLISION,
};
use crate::model::{CHVArchitecture, Network};

/// Run all layer-1 static checks against `model`. Order of findings within
/// the returned vector is stable for any given model.
pub fn run_static_checks(model: &CHVArchitecture) -> Vec<Finding> {
    let mut findings = Vec::new();

    check_duplicate_names(model, &mut findings);
    check_missing_references(model, &mut findings);
    check_invalid_cidrs(model, &mut findings);
    check_cidr_overlap(model, &mut findings);
    check_ip_attachment(model, &mut findings);
    check_gateway_in_network(model, &mut findings);
    check_dhcp_ranges(model, &mut findings);
    check_static_ip_in_dhcp_range(model, &mut findings);
    check_duplicate_ips(model, &mut findings);
    check_raw_secrets(model, &mut findings);
    check_role_permissions(model, &mut findings);
    check_invalid_edges(model, &mut findings);
    check_user_namespace(model, &mut findings);

    findings
}

// ---------------------------------------------------------------------------
// 1. DUPLICATE_NAME
// ---------------------------------------------------------------------------

fn check_duplicate_names(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    fn check<T, F: Fn(&T) -> &str>(
        items: &[T],
        section: &'static str,
        name_of: F,
        out: &mut Vec<Finding>,
    ) {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (idx, item) in items.iter().enumerate() {
            let n = name_of(item);
            if let Some(prev_idx) = seen.get(n) {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(DUPLICATE_NAME),
                    message: format!(
                        "duplicate name '{n}' in {section}: items [{prev_idx}] and [{idx}]"
                    ),
                    path: Some(format!("{section}[{idx}].name")),
                    resource_ref: Some(format!("{section}/{n}")),
                    blocking: true,
                    suggestion: Some(format!("rename one of the {section} entries to be unique")),
                });
            } else {
                seen.insert(n, idx);
            }
        }
    }

    check(&model.servers, "servers", |s| s.name.as_str(), out);
    check(&model.networks, "networks", |s| s.name.as_str(), out);
    check(&model.datastores, "datastores", |s| s.name.as_str(), out);
    check(
        &model.backup_targets,
        "backup_targets",
        |s| s.name.as_str(),
        out,
    );
    check(
        &model.backup_policies,
        "backup_policies",
        |s| s.name.as_str(),
        out,
    );
    check(&model.images, "images", |s| s.name.as_str(), out);
    check(&model.templates, "templates", |s| s.name.as_str(), out);
    check(&model.instances, "instances", |s| s.name.as_str(), out);
    check(&model.ssh_keys, "ssh_keys", |s| s.name.as_str(), out);
    check(
        &model.instance_users,
        "instance_users",
        |s| s.name.as_str(),
        out,
    );
    check(&model.roles, "roles", |s| s.name.as_str(), out);
    check(&model.users, "users", |s| s.name.as_str(), out);
    check(&model.projects, "projects", |s| s.name.as_str(), out);
}

// ---------------------------------------------------------------------------
// 2. MISSING_REFERENCE
// ---------------------------------------------------------------------------

fn check_missing_references(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let server_names: HashSet<&str> = model.servers.iter().map(|x| x.name.as_str()).collect();
    let datastore_names: HashSet<&str> = model.datastores.iter().map(|x| x.name.as_str()).collect();
    let network_names: HashSet<&str> = model.networks.iter().map(|x| x.name.as_str()).collect();
    let image_names: HashSet<&str> = model.images.iter().map(|x| x.name.as_str()).collect();
    let template_names: HashSet<&str> = model.templates.iter().map(|x| x.name.as_str()).collect();
    let instance_user_names: HashSet<&str> = model
        .instance_users
        .iter()
        .map(|x| x.name.as_str())
        .collect();
    let backup_policy_names: HashSet<&str> = model
        .backup_policies
        .iter()
        .map(|x| x.name.as_str())
        .collect();
    let backup_target_names: HashSet<&str> = model
        .backup_targets
        .iter()
        .map(|x| x.name.as_str())
        .collect();
    let role_names: HashSet<&str> = model.roles.iter().map(|x| x.name.as_str()).collect();
    let ssh_key_names: HashSet<&str> = model.ssh_keys.iter().map(|x| x.name.as_str()).collect();

    fn emit(
        out: &mut Vec<Finding>,
        section: &str,
        value: &str,
        path: String,
        ref_: Option<String>,
    ) {
        out.push(Finding {
            severity: Severity::Error,
            code: Cow::Borrowed(MISSING_REFERENCE),
            message: format!("reference to unknown {section} '{value}'"),
            path: Some(path),
            resource_ref: ref_,
            blocking: true,
            suggestion: Some(format!(
                "create a {section} named '{value}' or change the reference"
            )),
        });
    }

    // images.datastore -> datastores
    for (i, img) in model.images.iter().enumerate() {
        if let Some(ds) = &img.datastore {
            if !datastore_names.contains(ds.as_str()) {
                emit(
                    out,
                    "datastore",
                    ds,
                    format!("images[{i}].datastore"),
                    Some(format!("images/{}", img.name)),
                );
            }
        }
    }

    // templates.image -> images, templates.datastore -> datastores, templates.network -> networks
    for (i, tpl) in model.templates.iter().enumerate() {
        if !image_names.contains(tpl.image.as_str()) {
            emit(
                out,
                "image",
                &tpl.image,
                format!("templates[{i}].image"),
                Some(format!("templates/{}", tpl.name)),
            );
        }
        if let Some(ds) = &tpl.datastore {
            if !datastore_names.contains(ds.as_str()) {
                emit(
                    out,
                    "datastore",
                    ds,
                    format!("templates[{i}].datastore"),
                    Some(format!("templates/{}", tpl.name)),
                );
            }
        }
        if let Some(net) = &tpl.network {
            if !network_names.contains(net.as_str()) {
                emit(
                    out,
                    "network",
                    net,
                    format!("templates[{i}].network"),
                    Some(format!("templates/{}", tpl.name)),
                );
            }
        }
    }

    // backup_policies.target -> backup_targets
    for (i, bp) in model.backup_policies.iter().enumerate() {
        if !backup_target_names.contains(bp.target.as_str()) {
            emit(
                out,
                "backup_target",
                &bp.target,
                format!("backup_policies[{i}].target"),
                Some(format!("backup_policies/{}", bp.name)),
            );
        }
    }

    // instances cross-refs
    for (i, inst) in model.instances.iter().enumerate() {
        let inst_ref = format!("instances/{}", inst.name);
        if let Some(t) = &inst.template {
            if !template_names.contains(t.as_str()) {
                emit(
                    out,
                    "template",
                    t,
                    format!("instances[{i}].template"),
                    Some(inst_ref.clone()),
                );
            }
        }
        if let Some(p) = &inst.placement {
            if let Some(server) = &p.server {
                if !server_names.contains(server.as_str()) {
                    emit(
                        out,
                        "server",
                        server,
                        format!("instances[{i}].placement.server"),
                        Some(inst_ref.clone()),
                    );
                }
            }
        }
        for (di, disk) in inst.disks.iter().enumerate() {
            if let Some(ds) = &disk.datastore {
                if !datastore_names.contains(ds.as_str()) {
                    emit(
                        out,
                        "datastore",
                        ds,
                        format!("instances[{i}].disks[{di}].datastore"),
                        Some(inst_ref.clone()),
                    );
                }
            }
        }
        for (ni, net) in inst.networks.iter().enumerate() {
            if !network_names.contains(net.name.as_str()) {
                emit(
                    out,
                    "network",
                    &net.name,
                    format!("instances[{i}].networks[{ni}].name"),
                    Some(inst_ref.clone()),
                );
            }
        }
        if let Some(ci) = &inst.cloud_init {
            for (ui, u) in ci.users.iter().enumerate() {
                if let Some(r) = &u.ref_ {
                    if !instance_user_names.contains(r.as_str()) {
                        emit(
                            out,
                            "instance_user",
                            r,
                            format!("instances[{i}].cloud_init.users[{ui}].ref"),
                            Some(inst_ref.clone()),
                        );
                    }
                }
            }
        }
        if let Some(b) = &inst.backup {
            if let Some(p) = &b.policy {
                if !backup_policy_names.contains(p.as_str()) {
                    emit(
                        out,
                        "backup_policy",
                        p,
                        format!("instances[{i}].backup.policy"),
                        Some(inst_ref.clone()),
                    );
                }
            }
        }
    }

    // instance_users.ssh_authorized_keys[].ref -> ssh_keys
    for (i, u) in model.instance_users.iter().enumerate() {
        for (ki, k) in u.ssh_authorized_keys.iter().enumerate() {
            if let Some(r) = &k.ref_ {
                if !ssh_key_names.contains(r.as_str()) {
                    emit(
                        out,
                        "ssh_key",
                        r,
                        format!("instance_users[{i}].ssh_authorized_keys[{ki}].ref"),
                        Some(format!("instance_users/{}", u.name)),
                    );
                }
            }
        }
    }

    // users.roles[] -> roles
    for (i, u) in model.users.iter().enumerate() {
        for (ri, role) in u.roles.iter().enumerate() {
            if !role_names.contains(role.as_str()) {
                emit(
                    out,
                    "role",
                    role,
                    format!("users[{i}].roles[{ri}]"),
                    Some(format!("users/{}", u.name)),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. INVALID_CIDR
// ---------------------------------------------------------------------------

fn check_invalid_cidrs(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    for (i, net) in model.networks.iter().enumerate() {
        if let Some(cidr) = &net.cidr {
            if cidr.parse::<IpNetwork>().is_err() {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(INVALID_CIDR),
                    message: format!("network '{}' has invalid CIDR: {cidr}", net.name),
                    path: Some(format!("networks[{i}].cidr")),
                    resource_ref: Some(format!("networks/{}", net.name)),
                    blocking: true,
                    suggestion: Some("provide a syntactically valid IPv4 or IPv6 CIDR".to_string()),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. NETWORK_CIDR_OVERLAP
// ---------------------------------------------------------------------------

fn cidr_of(net: &Network) -> Option<IpNetwork> {
    net.cidr.as_deref().and_then(|s| s.parse().ok())
}

fn networks_overlap(a: &IpNetwork, b: &IpNetwork) -> bool {
    // Two networks overlap if either's first/last address is within the other.
    a.contains(b.network()) || b.contains(a.network())
}

fn check_cidr_overlap(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let parsed: Vec<(usize, &Network, IpNetwork)> = model
        .networks
        .iter()
        .enumerate()
        .filter_map(|(i, n)| cidr_of(n).map(|c| (i, n, c)))
        .collect();

    for ai in 0..parsed.len() {
        for bi in (ai + 1)..parsed.len() {
            let (i, a, ac) = (parsed[ai].0, parsed[ai].1, parsed[ai].2);
            let (j, b, bc) = (parsed[bi].0, parsed[bi].1, parsed[bi].2);
            if networks_overlap(&ac, &bc) {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(NETWORK_CIDR_OVERLAP),
                    message: format!(
                        "network '{}' ({}) overlaps with '{}' ({})",
                        a.name, ac, b.name, bc
                    ),
                    path: Some(format!("networks[{j}].cidr")),
                    resource_ref: Some(format!("networks/{}", b.name)),
                    blocking: true,
                    suggestion: Some(format!(
                        "make CIDRs disjoint or merge networks '{}' and '{}'",
                        a.name, b.name
                    )),
                });
                let _ = i; // suppress unused; index is captured in path of outer
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 5. IP_OUTSIDE_NETWORK
// ---------------------------------------------------------------------------

fn check_ip_attachment(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let net_cidrs: HashMap<&str, IpNetwork> = model
        .networks
        .iter()
        .filter_map(|n| cidr_of(n).map(|c| (n.name.as_str(), c)))
        .collect();

    for (i, inst) in model.instances.iter().enumerate() {
        for (ni, attach) in inst.networks.iter().enumerate() {
            let Some(ip_str) = &attach.ip else { continue };
            let Ok(ip) = ip_str.parse::<IpAddr>() else {
                continue;
            };
            let Some(cidr) = net_cidrs.get(attach.name.as_str()) else {
                continue;
            };
            if !cidr.contains(ip) {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(IP_OUTSIDE_NETWORK),
                    message: format!(
                        "instance '{}' attaches to network '{}' with IP {ip} outside CIDR {cidr}",
                        inst.name, attach.name
                    ),
                    path: Some(format!("instances[{i}].networks[{ni}].ip")),
                    resource_ref: Some(format!("instances/{}", inst.name)),
                    blocking: true,
                    suggestion: Some(format!(
                        "use an IP inside {cidr} or attach to a different network"
                    )),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. GATEWAY_OUTSIDE_NETWORK
// ---------------------------------------------------------------------------

fn check_gateway_in_network(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    for (i, net) in model.networks.iter().enumerate() {
        let Some(gw_str) = &net.gateway else { continue };
        let Ok(gw) = gw_str.parse::<IpAddr>() else {
            // Malformed gateway is reported via INVALID_CIDR-adjacent
            // diagnostics elsewhere; we don't double-report here.
            continue;
        };
        let Some(cidr) = cidr_of(net) else { continue };
        if !cidr.contains(gw) {
            out.push(Finding {
                severity: Severity::Error,
                code: Cow::Borrowed(GATEWAY_OUTSIDE_NETWORK),
                message: format!("network '{}' gateway {gw} is outside CIDR {cidr}", net.name),
                path: Some(format!("networks[{i}].gateway")),
                resource_ref: Some(format!("networks/{}", net.name)),
                blocking: true,
                suggestion: Some(format!("set gateway to an address inside {cidr}")),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 7. DHCP_RANGE_INVALID
// ---------------------------------------------------------------------------

fn check_dhcp_ranges(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    for (i, net) in model.networks.iter().enumerate() {
        let Some(dhcp) = &net.dhcp else { continue };
        if !dhcp.enabled {
            continue;
        }
        let net_cidr = cidr_of(net);
        let start = dhcp
            .range_start
            .as_deref()
            .and_then(|s| s.parse::<IpAddr>().ok());
        let end = dhcp
            .range_end
            .as_deref()
            .and_then(|s| s.parse::<IpAddr>().ok());

        // If both bounds parse and the network has a CIDR, perform full check.
        match (start, end, net_cidr) {
            (Some(s), Some(e), Some(c)) => {
                if !c.contains(s) || !c.contains(e) {
                    out.push(Finding {
                        severity: Severity::Error,
                        code: Cow::Borrowed(DHCP_RANGE_INVALID),
                        message: format!(
                            "network '{}' DHCP range {s}..{e} is outside CIDR {c}",
                            net.name
                        ),
                        path: Some(format!("networks[{i}].dhcp")),
                        resource_ref: Some(format!("networks/{}", net.name)),
                        blocking: true,
                        suggestion: Some(format!("place start and end inside {c}")),
                    });
                } else if !ip_ordered(s, e) {
                    out.push(Finding {
                        severity: Severity::Error,
                        code: Cow::Borrowed(DHCP_RANGE_INVALID),
                        message: format!(
                            "network '{}' DHCP range_start {s} is greater than range_end {e}",
                            net.name
                        ),
                        path: Some(format!("networks[{i}].dhcp")),
                        resource_ref: Some(format!("networks/{}", net.name)),
                        blocking: true,
                        suggestion: Some("ensure range_start <= range_end".to_string()),
                    });
                }
            }
            // Partial / missing bounds with DHCP enabled is also invalid.
            (None, _, _) | (_, None, _) => {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(DHCP_RANGE_INVALID),
                    message: format!(
                        "network '{}' has dhcp.enabled but missing or unparseable range_start/range_end",
                        net.name
                    ),
                    path: Some(format!("networks[{i}].dhcp")),
                    resource_ref: Some(format!("networks/{}", net.name)),
                    blocking: true,
                    suggestion: Some("set dhcp.range_start and dhcp.range_end as IP addresses".to_string()),
                });
            }
            _ => {} // No CIDR: cidr-validity reported elsewhere
        }
    }
}

fn ip_ordered(a: IpAddr, b: IpAddr) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a <= b,
        (IpAddr::V6(a), IpAddr::V6(b)) => a <= b,
        _ => false, // mismatched families — caller should treat as invalid
    }
}

fn ip_in_range(ip: IpAddr, start: IpAddr, end: IpAddr) -> bool {
    ip_ordered(start, ip) && ip_ordered(ip, end)
}

// ---------------------------------------------------------------------------
// 13. STATIC_IP_IN_DHCP_RANGE (warning)
// ---------------------------------------------------------------------------

fn check_static_ip_in_dhcp_range(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let net_dhcp: HashMap<&str, (IpAddr, IpAddr)> = model
        .networks
        .iter()
        .filter_map(|n| {
            let d = n.dhcp.as_ref()?;
            if !d.enabled {
                return None;
            }
            let s = d.range_start.as_deref()?.parse::<IpAddr>().ok()?;
            let e = d.range_end.as_deref()?.parse::<IpAddr>().ok()?;
            Some((n.name.as_str(), (s, e)))
        })
        .collect();

    for (i, inst) in model.instances.iter().enumerate() {
        for (ni, attach) in inst.networks.iter().enumerate() {
            let Some(ip_str) = &attach.ip else { continue };
            let Ok(ip) = ip_str.parse::<IpAddr>() else {
                continue;
            };
            let Some(&(start, end)) = net_dhcp.get(attach.name.as_str()) else {
                continue;
            };
            if ip_in_range(ip, start, end) {
                out.push(Finding {
                    severity: Severity::Warning,
                    code: Cow::Borrowed(STATIC_IP_IN_DHCP_RANGE),
                    message: format!(
                        "instance '{}' static IP {ip} on '{}' falls inside DHCP range {start}..{end}",
                        inst.name, attach.name
                    ),
                    path: Some(format!("instances[{i}].networks[{ni}].ip")),
                    resource_ref: Some(format!("instances/{}", inst.name)),
                    blocking: false,
                    suggestion: Some(
                        "reserve this IP outside the DHCP range, or shrink the DHCP range".to_string(),
                    ),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8. DUPLICATE_IP
// ---------------------------------------------------------------------------

fn check_duplicate_ips(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    // Collect (path, ip) entries; emit DUPLICATE_IP for each subsequent
    // occurrence so the user sees every collision pair.
    let mut seen: HashMap<IpAddr, String> = HashMap::new();

    let record =
        |path: String, ip_str: &str, out: &mut Vec<Finding>, seen: &mut HashMap<IpAddr, String>| {
            let Ok(ip) = ip_str.parse::<IpAddr>() else {
                return;
            };
            if let Some(prev_path) = seen.get(&ip) {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(DUPLICATE_IP),
                    message: format!("duplicate IP {ip}: also used at {prev_path}"),
                    path: Some(path.clone()),
                    resource_ref: None,
                    blocking: true,
                    suggestion: Some("assign a unique IP to each interface".to_string()),
                });
            } else {
                seen.insert(ip, path);
            }
        };

    for (i, net) in model.networks.iter().enumerate() {
        if let Some(gw) = &net.gateway {
            record(format!("networks[{i}].gateway"), gw, out, &mut seen);
        }
    }
    for (i, inst) in model.instances.iter().enumerate() {
        for (ni, attach) in inst.networks.iter().enumerate() {
            if let Some(ip) = &attach.ip {
                record(
                    format!("instances[{i}].networks[{ni}].ip"),
                    ip,
                    out,
                    &mut seen,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 9. RAW_SECRET_FORBIDDEN
// ---------------------------------------------------------------------------

/// Set of leaf field names considered secret-bearing. A non-empty
/// (`!= ""`) string at any of these paths emits `RAW_SECRET_FORBIDDEN`;
/// callers must use a `secret_ref` field instead.
const SECRET_FIELD_NAMES: &[&str] = &["password", "token", "private_key", "secret"];

fn check_raw_secrets(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    // We also walk the `extras` map of `Project` (the only free-form
    // section) and any other deserialized-but-uncovered string field. The
    // strongly-typed model itself only carries password/token at known
    // sites; we hand-list those, then fall back to a generic walk for
    // `Project.extras`.

    for (i, u) in model.users.iter().enumerate() {
        if let Some(p) = &u.password {
            if !p.is_empty() {
                out.push(make_secret_finding(
                    format!("users[{i}].password"),
                    Some(format!("users/{}", u.name)),
                ));
            }
        }
        if let Some(t) = &u.token {
            if !t.is_empty() {
                out.push(make_secret_finding(
                    format!("users[{i}].token"),
                    Some(format!("users/{}", u.name)),
                ));
            }
        }
    }

    for (i, u) in model.instance_users.iter().enumerate() {
        if let Some(p) = &u.password {
            if !p.is_empty() {
                out.push(make_secret_finding(
                    format!("instance_users[{i}].password"),
                    Some(format!("instance_users/{}", u.name)),
                ));
            }
        }
    }

    // Project extras — recursive walk of arbitrary YAML.
    for (i, proj) in model.projects.iter().enumerate() {
        for (k, v) in &proj.extras {
            walk_yaml_for_secrets(
                v,
                &format!("projects[{i}].{k}"),
                k,
                &format!("projects/{}", proj.name),
                out,
            );
        }
    }
}

fn make_secret_finding(path: String, resource_ref: Option<String>) -> Finding {
    Finding {
        severity: Severity::Error,
        code: Cow::Borrowed(RAW_SECRET_FORBIDDEN),
        message: format!(
            "raw secret value found at {path}; use a secret_ref instead"
        ),
        path: Some(path),
        resource_ref,
        blocking: true,
        suggestion: Some(
            "remove the literal secret and replace it with a secret_ref pointing at a managed secret".to_string(),
        ),
    }
}

fn walk_yaml_for_secrets(
    v: &serde_json::Value,
    path: &str,
    leaf_name: &str,
    resource_ref: &str,
    out: &mut Vec<Finding>,
) {
    match v {
        serde_json::Value::String(s)
            if !s.is_empty() && SECRET_FIELD_NAMES.contains(&leaf_name) =>
        {
            out.push(make_secret_finding(
                path.to_string(),
                Some(resource_ref.to_string()),
            ));
        }
        serde_json::Value::Object(m) => {
            for (k, val) in m {
                let new_path = format!("{path}.{k}");
                walk_yaml_for_secrets(val, &new_path, k, resource_ref, out);
            }
        }
        serde_json::Value::Array(seq) => {
            for (i, item) in seq.iter().enumerate() {
                let new_path = format!("{path}[{i}]");
                walk_yaml_for_secrets(item, &new_path, leaf_name, resource_ref, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 10. INVALID_PERMISSION
// ---------------------------------------------------------------------------

fn check_role_permissions(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let allowed: HashSet<&str> = ALLOWED_PERMISSIONS.iter().copied().collect();
    for (i, role) in model.roles.iter().enumerate() {
        for (pi, perm) in role.permissions.iter().enumerate() {
            if !allowed.contains(perm.as_str()) {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(INVALID_PERMISSION),
                    message: format!(
                        "role '{}' has unknown permission '{perm}'",
                        role.name
                    ),
                    path: Some(format!("roles[{i}].permissions[{pi}]")),
                    resource_ref: Some(format!("roles/{}", role.name)),
                    blocking: true,
                    suggestion: Some(
                        "use one of the canonical CHV permission strings (see codes::ALLOWED_PERMISSIONS)".to_string(),
                    ),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 11. INVALID_EDGE
// ---------------------------------------------------------------------------

fn check_invalid_edges(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    // INVALID_EDGE is reserved for cases where an implicit edge in the YAML
    // points at an entity from a wrong section that *does* exist by name.
    // Phase 1 covers two such cases:
    //   - instance.placement.server names something that exists, but the
    //     name resolves to a non-server section.
    //   - instance.networks[].name resolves to a non-network section.
    //
    // (Pure "doesn't exist" cases are already MISSING_REFERENCE.)
    let server_names: HashSet<&str> = model.servers.iter().map(|x| x.name.as_str()).collect();
    let network_names: HashSet<&str> = model.networks.iter().map(|x| x.name.as_str()).collect();

    let datastore_names: HashSet<&str> = model.datastores.iter().map(|x| x.name.as_str()).collect();

    for (i, inst) in model.instances.iter().enumerate() {
        if let Some(p) = &inst.placement {
            if let Some(server) = &p.server {
                if !server_names.contains(server.as_str())
                    && (network_names.contains(server.as_str())
                        || datastore_names.contains(server.as_str()))
                {
                    out.push(Finding {
                        severity: Severity::Error,
                        code: Cow::Borrowed(INVALID_EDGE),
                        message: format!(
                            "instance '{}' placed_on '{server}' resolves to a non-server resource",
                            inst.name
                        ),
                        path: Some(format!("instances[{i}].placement.server")),
                        resource_ref: Some(format!("instances/{}", inst.name)),
                        blocking: true,
                        suggestion: Some("placement.server must reference a server".to_string()),
                    });
                }
            }
        }
        for (ni, attach) in inst.networks.iter().enumerate() {
            if !network_names.contains(attach.name.as_str())
                && (server_names.contains(attach.name.as_str())
                    || datastore_names.contains(attach.name.as_str()))
            {
                out.push(Finding {
                    severity: Severity::Error,
                    code: Cow::Borrowed(INVALID_EDGE),
                    message: format!(
                        "instance '{}' network attachment '{}' resolves to a non-network resource",
                        inst.name, attach.name
                    ),
                    path: Some(format!("instances[{i}].networks[{ni}].name")),
                    resource_ref: Some(format!("instances/{}", inst.name)),
                    blocking: true,
                    suggestion: Some("attached_to must reference a network".to_string()),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 12. USER_NAMESPACE_COLLISION
// ---------------------------------------------------------------------------

fn check_user_namespace(model: &CHVArchitecture, out: &mut Vec<Finding>) {
    let platform: HashSet<&str> = model.users.iter().map(|x| x.name.as_str()).collect();
    for (i, iu) in model.instance_users.iter().enumerate() {
        if platform.contains(iu.name.as_str()) {
            out.push(Finding {
                severity: Severity::Error,
                code: Cow::Borrowed(USER_NAMESPACE_COLLISION),
                message: format!(
                    "name '{}' appears in both users[] (platform) and instance_users[]",
                    iu.name
                ),
                path: Some(format!("instance_users[{i}].name")),
                resource_ref: Some(format!("instance_users/{}", iu.name)),
                blocking: true,
                suggestion: Some(
                    "rename either the platform user or the instance user; the namespaces must be disjoint"
                        .to_string(),
                ),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_yaml;

    fn first_code(findings: &[Finding]) -> Option<&str> {
        findings.first().map(|f| f.code.as_ref())
    }

    fn assert_only_code(findings: &[Finding], code: &str) {
        assert!(
            !findings.is_empty(),
            "expected at least one '{code}' finding, got none"
        );
        for f in findings {
            assert_eq!(
                f.code.as_ref(),
                code,
                "expected only '{code}' findings, got {:?}",
                f
            );
        }
    }

    fn parse(s: &str) -> CHVArchitecture {
        parse_yaml(s).expect("parse fixture")
    }

    #[test]
    fn no_findings_for_minimal_model() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
"#,
        );
        let f = run_static_checks(&m);
        assert!(f.is_empty(), "got findings: {f:#?}");
    }

    #[test]
    fn duplicate_name_in_networks() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: net-a
    type: bridge
  - name: net-a
    type: bridge
"#,
        );
        let f = run_static_checks(&m);
        let dups: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == DUPLICATE_NAME)
            .collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].path.as_deref(), Some("networks[1].name"));
    }

    #[test]
    fn missing_reference_template_image() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
templates:
  - name: small
    image: nonexistent-image
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == MISSING_REFERENCE)
            .collect();
        assert!(!r.is_empty());
        assert!(r
            .iter()
            .any(|x| x.path.as_deref() == Some("templates[0].image")));
    }

    #[test]
    fn invalid_cidr_emits_finding() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: bad-net
    type: bridge
    cidr: 999.0.0.0/24
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == INVALID_CIDR)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn cidr_overlap_pair() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: outer
    type: bridge
    cidr: 10.0.0.0/16
  - name: inner
    type: bridge
    cidr: 10.0.1.0/24
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == NETWORK_CIDR_OVERLAP)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn duplicate_ip_emits_finding() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
instances:
  - name: a
    networks:
      - name: n
        ip: 10.0.0.5
  - name: b
    networks:
      - name: n
        ip: 10.0.0.5
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == DUPLICATE_IP)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn ip_outside_network_emits_finding() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
instances:
  - name: a
    networks:
      - name: n
        ip: 10.99.0.5
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == IP_OUTSIDE_NETWORK)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn gateway_outside_network_emits_finding() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
    gateway: 192.168.1.1
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == GATEWAY_OUTSIDE_NETWORK)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn dhcp_range_invalid_start_after_end() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
    dhcp:
      enabled: true
      range_start: 10.0.0.200
      range_end: 10.0.0.100
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == DHCP_RANGE_INVALID)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn dhcp_range_outside_network() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
    dhcp:
      enabled: true
      range_start: 10.99.0.10
      range_end: 10.99.0.20
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == DHCP_RANGE_INVALID)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn raw_secret_in_user_password() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
users:
  - name: u
    password: hunter2
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == RAW_SECRET_FORBIDDEN)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn invalid_permission_in_role() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
roles:
  - name: r
    permissions:
      - bogus:permission
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == INVALID_PERMISSION)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn invalid_edge_placement_points_at_network() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: my-net
    type: bridge
instances:
  - name: a
    placement:
      server: my-net
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == INVALID_EDGE)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn user_namespace_collision_blocks() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
users:
  - name: shared-name
instance_users:
  - name: shared-name
"#,
        );
        let f = run_static_checks(&m);
        let r: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == USER_NAMESPACE_COLLISION)
            .collect();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn static_ip_in_dhcp_range_warns() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: n
    type: bridge
    cidr: 10.0.0.0/24
    dhcp:
      enabled: true
      range_start: 10.0.0.100
      range_end: 10.0.0.200
instances:
  - name: a
    networks:
      - name: n
        ip: 10.0.0.150
"#,
        );
        let f = run_static_checks(&m);
        let warns: Vec<_> = f
            .iter()
            .filter(|x| x.code.as_ref() == STATIC_IP_IN_DHCP_RANGE)
            .collect();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].severity, Severity::Warning);
        assert!(!warns[0].blocking);
    }

    #[test]
    fn ip_ordered_handles_mismatched_families() {
        let v4a: IpAddr = "10.0.0.1".parse().unwrap();
        let v4b: IpAddr = "10.0.0.2".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        assert!(ip_ordered(v4a, v4b));
        assert!(!ip_ordered(v4b, v4a));
        assert!(!ip_ordered(v4a, v6), "mismatched families return false");
        assert!(!ip_ordered(v6, v4a), "mismatched families return false");
    }

    #[test]
    fn networks_overlap_detects_subset() {
        let outer: IpNetwork = "10.0.0.0/16".parse().unwrap();
        let inner: IpNetwork = "10.0.1.0/24".parse().unwrap();
        let elsewhere: IpNetwork = "192.168.0.0/16".parse().unwrap();
        assert!(networks_overlap(&outer, &inner));
        assert!(networks_overlap(&inner, &outer));
        assert!(!networks_overlap(&outer, &elsewhere));
    }

    /// Sanity check: invoking the full pipeline twice on the same input
    /// produces equal results (no hidden state, no nondeterminism).
    #[test]
    fn deterministic_output() {
        let yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
networks:
  - name: a
    type: bridge
    cidr: 10.0.0.0/24
  - name: b
    type: bridge
    cidr: 10.0.0.0/16
"#;
        let m = parse(yaml);
        let a = run_static_checks(&m);
        let b = run_static_checks(&m);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.code, y.code);
            assert_eq!(x.path, y.path);
        }
    }

    /// Negative case: empty model produces no findings.
    #[test]
    fn empty_model_zero_findings() {
        let m = CHVArchitecture {
            api_version: crate::EXPECTED_API_VERSION.to_string(),
            kind: crate::EXPECTED_KIND.to_string(),
            metadata: crate::model::Metadata {
                name: "x".to_string(),
                display_name: None,
                description: None,
                environment: None,
                owner: None,
                labels: Default::default(),
            },
            servers: vec![],
            networks: vec![],
            datastores: vec![],
            backup_targets: vec![],
            backup_policies: vec![],
            images: vec![],
            templates: vec![],
            instances: vec![],
            ssh_keys: vec![],
            instance_users: vec![],
            roles: vec![],
            users: vec![],
            projects: vec![],
        };
        let f = run_static_checks(&m);
        assert!(f.is_empty(), "{f:#?}");
    }

    #[test]
    fn first_code_helper_smoke() {
        let m = parse(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
roles:
  - name: r
    permissions:
      - nope
"#,
        );
        let f = run_static_checks(&m);
        assert_eq!(first_code(&f), Some(INVALID_PERMISSION));
        assert_only_code(&f, INVALID_PERMISSION);
    }
}
