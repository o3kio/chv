use chv_errors::ChvError;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::info;

const RUNTIME_DIR: &str = "/run/chv/nwd";

fn conf_path(network_id: &str) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.conf", network_id))
}

fn pid_path(network_id: &str) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.pid", network_id))
}

fn hosts_path(network_id: &str) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.hosts", network_id))
}

async fn is_dnsmasq_running(network_id: &str) -> bool {
    let pid_file = pid_path(network_id);
    let Ok(pid_str) = tokio::fs::read_to_string(&pid_file).await else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<i32>() else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn reload_dnsmasq(network_id: &str) {
    let pid_file = pid_path(network_id);
    let Ok(pid_str) = tokio::fs::read_to_string(&pid_file).await else {
        return;
    };
    let _ = Command::new("kill")
        .args(["-HUP", pid_str.trim()])
        .output()
        .await;
}

/// Interface name recorded in a dnsmasq scope.
///
/// An existing config is the source of truth during upgrades: it records the
/// interface the scope was created against. A first-time scope derives the
/// same `br-*` name as the executor
/// ([`chv_common::bridge_name_for_network`]), which truncates and hashes ids
/// that do not fit the 15-character IFNAMSIZ limit. `format!("br-{network_id}")`
/// is NOT equivalent once ids exceed 12 characters.
fn interface_name_for_network(existing: &str, network_id: &str) -> String {
    existing
        .lines()
        .find(|l| l.starts_with("interface="))
        .and_then(|l| l.strip_prefix("interface="))
        .map(|s| s.to_string())
        .unwrap_or_else(|| chv_common::bridge_name_for_network(network_id))
}

pub async fn ensure_dhcp_scope(
    network_id: &str,
    cidr: &str,
    range_start: &str,
    range_end: &str,
    dns_servers: &[String],
) -> Result<(), ChvError> {
    // Reject network_id values that could escape the runtime directory or inject shell commands.
    // Mirrors the allowlist in executor.rs::sanitize_id.
    if network_id.is_empty()
        || !network_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(ChvError::InvalidArgument {
            field: "network_id".to_string(),
            reason: format!("network_id contains invalid characters: {network_id}"),
        });
    }

    if cidr.is_empty() || range_start.is_empty() || range_end.is_empty() {
        return Err(ChvError::InvalidArgument {
            field: "dhcp_scope".to_string(),
            reason: "cidr, range_start, and range_end are required".to_string(),
        });
    }

    // Validate IPs
    if range_start.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(ChvError::InvalidArgument {
            field: "range_start".to_string(),
            reason: format!("invalid IPv4 address: '{}'", range_start),
        });
    }
    if range_end.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(ChvError::InvalidArgument {
            field: "range_end".to_string(),
            reason: format!("invalid IPv4 address: '{}'", range_end),
        });
    }

    // Validate range ordering and subnet membership
    validate_dhcp_range(cidr, range_start, range_end)?;

    let netmask = cidr_to_netmask(cidr)?;

    let conf = conf_path(network_id);
    let hosts = hosts_path(network_id);
    let pid = pid_path(network_id);

    let _ = tokio::fs::create_dir_all(RUNTIME_DIR).await;

    // Read existing config to detect if scope changed
    let existing = tokio::fs::read_to_string(&conf).await.unwrap_or_default();
    let dhcp_range_line = format!("dhcp-range={},{},{},12h", range_start, range_end, netmask);

    if existing.contains(&dhcp_range_line) && is_dnsmasq_running(network_id).await {
        info!(network_id = %network_id, "DHCP scope unchanged, skipping reload");
        return Ok(());
    }

    // Reuse the name already recorded in the config; otherwise derive the
    // same IFNAMSIZ-safe bridge name the executor creates.
    let bridge_name = interface_name_for_network(&existing, network_id);

    // Derive gateway from CIDR (first usable IP)
    let gateway = derive_gateway(cidr).unwrap_or_default();

    // Use provided DNS servers or default
    let dns_option = if dns_servers.is_empty() {
        "1.1.1.1".to_string()
    } else {
        dns_servers.join(",")
    };

    let config = format!(
        "interface={}\nbind-interfaces\nport=0\ndhcp-range={},{},{},12h\ndhcp-option=3,{}\ndhcp-option=6,{}\ndhcp-hostsfile={}\nexcept-interface=lo\nno-resolv\n",
        bridge_name, range_start, range_end, netmask, gateway, dns_option, hosts.display()
    );

    tokio::fs::write(&conf, &config)
        .await
        .map_err(|e| ChvError::Io {
            path: conf.to_string_lossy().to_string(),
            source: e,
        })?;

    // Ensure hosts file exists
    if tokio::fs::metadata(&hosts).await.is_err() {
        let _ = tokio::fs::write(&hosts, "").await;
    }

    if is_dnsmasq_running(network_id).await {
        // Reload running instance
        reload_dnsmasq(network_id).await;
        info!(network_id = %network_id, "DHCP scope updated, dnsmasq reloaded");
    } else {
        // Start new dnsmasq
        let out = Command::new("dnsmasq")
            .args([
                &format!("--conf-file={}", conf.display()),
                &format!("--pid-file={}", pid.display()),
            ])
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
                reason: format!("dnsmasq start failed: {}", stderr),
            });
        }
        info!(network_id = %network_id, "DHCP scope applied, dnsmasq started");
    }

    Ok(())
}

fn validate_dhcp_range(cidr: &str, range_start: &str, range_end: &str) -> Result<(), ChvError> {
    // Parse the subnet CIDR
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid CIDR notation: '{}'", cidr),
        })?;

    let prefix: u8 = prefix_str.parse().map_err(|_| ChvError::InvalidArgument {
        field: "cidr".to_string(),
        reason: format!("invalid prefix in CIDR: '{}'", cidr),
    })?;

    if prefix > 32 {
        return Err(ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("prefix {} exceeds 32", prefix),
        });
    }

    let subnet_ip: std::net::Ipv4Addr = ip_str.parse().map_err(|_| ChvError::InvalidArgument {
        field: "cidr".to_string(),
        reason: format!("invalid IP in CIDR: '{}'", cidr),
    })?;

    let start: std::net::Ipv4Addr = range_start.parse().map_err(|_| ChvError::InvalidArgument {
        field: "range_start".to_string(),
        reason: format!("invalid IPv4 address: '{}'", range_start),
    })?;
    let end: std::net::Ipv4Addr = range_end.parse().map_err(|_| ChvError::InvalidArgument {
        field: "range_end".to_string(),
        reason: format!("invalid IPv4 address: '{}'", range_end),
    })?;

    let start_u32 = u32::from(start);
    let end_u32 = u32::from(end);

    // Range must not be empty (start must be strictly less than end)
    if start_u32 >= end_u32 {
        return Err(ChvError::InvalidArgument {
            field: "dhcp_range".to_string(),
            reason: format!(
                "range_start '{}' must be less than range_end '{}'",
                range_start, range_end
            ),
        });
    }

    // Compute network/broadcast from CIDR
    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    let subnet_u32 = u32::from(subnet_ip);
    let network = subnet_u32 & mask;
    let broadcast = network | !mask;

    // Both endpoints must be within the subnet (exclusive of network and broadcast addresses)
    if start_u32 <= network {
        return Err(ChvError::InvalidArgument {
            field: "range_start".to_string(),
            reason: format!(
                "range_start '{}' is at or before the network address of '{}'",
                range_start, cidr
            ),
        });
    }
    if end_u32 >= broadcast {
        return Err(ChvError::InvalidArgument {
            field: "range_end".to_string(),
            reason: format!(
                "range_end '{}' is at or beyond the broadcast address of '{}'",
                range_end, cidr
            ),
        });
    }

    // Derive gateway (network + 1) and ensure range does not include it
    let gateway_u32 = network + 1;
    if start_u32 <= gateway_u32 && gateway_u32 <= end_u32 {
        return Err(ChvError::InvalidArgument {
            field: "dhcp_range".to_string(),
            reason: format!(
                "DHCP range '{}-{}' overlaps with the gateway address in subnet '{}'",
                range_start, range_end, cidr
            ),
        });
    }

    Ok(())
}

fn cidr_to_netmask(cidr: &str) -> Result<String, ChvError> {
    let prefix: u8 = cidr
        .split('/')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("invalid CIDR notation: '{}'", cidr),
        })?;

    if prefix > 32 {
        return Err(ChvError::InvalidArgument {
            field: "cidr".to_string(),
            reason: format!("prefix {} exceeds 32", prefix),
        });
    }

    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    Ok(format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xFF,
        (mask >> 16) & 0xFF,
        (mask >> 8) & 0xFF,
        mask & 0xFF,
    ))
}

fn derive_gateway(cidr: &str) -> Option<String> {
    let (ip_str, prefix_str) = cidr.split_once('/')?;
    let prefix: u8 = prefix_str.parse().ok()?;
    let ip: std::net::Ipv4Addr = ip_str.parse().ok()?;
    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    let network = u32::from(ip) & mask;
    let gateway = std::net::Ipv4Addr::from(network + 1);
    Some(gateway.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cidr_to_netmask() {
        assert_eq!(cidr_to_netmask("10.0.0.0/24").unwrap(), "255.255.255.0");
        assert_eq!(cidr_to_netmask("10.0.0.0/16").unwrap(), "255.255.0.0");
        assert_eq!(cidr_to_netmask("10.0.0.0/8").unwrap(), "255.0.0.0");
        assert_eq!(cidr_to_netmask("10.0.0.0/32").unwrap(), "255.255.255.255");
        assert!(cidr_to_netmask("10.0.0.0").is_err());
    }

    #[test]
    fn test_validate_dhcp_range_valid() {
        // Normal range well within a /24 subnet (skips .1 gateway)
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.10", "10.0.0.200").is_ok());
    }

    #[test]
    fn test_validate_dhcp_range_start_not_less_than_end() {
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.100", "10.0.0.50").is_err());
        // Equal start and end is also invalid
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.50", "10.0.0.50").is_err());
    }

    #[test]
    fn test_validate_dhcp_range_outside_subnet() {
        // range_start is in a different subnet
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.1.10", "10.0.1.200").is_err());
        // range_end is at the broadcast address
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.10", "10.0.0.255").is_err());
        // range_start is at/before the network address
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.0", "10.0.0.200").is_err());
    }

    #[test]
    fn test_validate_dhcp_range_overlaps_gateway() {
        // Gateway is 10.0.0.1 — range must not include it
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.1", "10.0.0.100").is_err());
        // Range starts after gateway: OK
        assert!(validate_dhcp_range("10.0.0.0/24", "10.0.0.2", "10.0.0.100").is_ok());
    }

    #[test]
    fn test_derive_gateway_canonical_cidr() {
        assert_eq!(derive_gateway("10.0.0.0/24"), Some("10.0.0.1".to_string()));
        assert_eq!(
            derive_gateway("192.168.1.0/24"),
            Some("192.168.1.1".to_string())
        );
    }

    #[test]
    fn test_derive_gateway_non_canonical_cidr() {
        // Non-canonical: host bits set. Gateway should still be network+1.
        assert_eq!(derive_gateway("10.0.0.5/24"), Some("10.0.0.1".to_string()));
        assert_eq!(
            derive_gateway("172.16.3.200/16"),
            Some("172.16.0.1".to_string())
        );
    }

    #[tokio::test]
    async fn ensure_dhcp_scope_rejects_path_traversal_network_id() {
        let result = ensure_dhcp_scope(
            "../../../etc",
            "192.168.1.0/24",
            "192.168.1.100",
            "192.168.1.200",
            &[],
        )
        .await;
        assert!(
            result.is_err(),
            "path traversal network_id must be rejected"
        );
    }

    #[tokio::test]
    async fn ensure_dhcp_scope_rejects_semicolon_injection() {
        let result = ensure_dhcp_scope(
            "net1;rm -rf /",
            "192.168.1.0/24",
            "192.168.1.100",
            "192.168.1.200",
            &[],
        )
        .await;
        assert!(
            result.is_err(),
            "shell-injection network_id must be rejected"
        );
    }

    #[test]
    fn interface_name_matches_the_executor_bridge_for_128_bit_ids() {
        let network_id = "0123456789abcdef0123456789abcdef";
        let name = interface_name_for_network("", network_id);
        assert_eq!(name, chv_common::bridge_name_for_network(network_id));
        assert_eq!(name.len(), 15, "must fit IFNAMSIZ: {name}");
        assert_ne!(name, format!("br-{network_id}"));
    }

    #[test]
    fn interface_name_reuses_the_recorded_config_value() {
        let existing = "interface=br-legacy0\nbind-interfaces\n";
        assert_eq!(
            interface_name_for_network(existing, "0123456789abcdef0123456789abcdef"),
            "br-legacy0"
        );
    }
}
