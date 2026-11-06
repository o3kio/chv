use chv_errors::ChvError;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::info;

const RUNTIME_DIR: &str = "/run/chv/nwd";

fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn conf_path(network_id: &str) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.conf", network_id))
}

fn pid_path(network_id: &str) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("dnsmasq-{}.pid", network_id))
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

pub async fn ensure_dns_scope(
    network_id: &str,
    forwarders: &[&str],
    static_records: &std::collections::HashMap<String, String>,
) -> Result<(), ChvError> {
    let conf = conf_path(network_id);

    let existing = tokio::fs::read_to_string(&conf).await.unwrap_or_default();
    if existing.is_empty() {
        return Err(ChvError::NetworkUnavailable {
            resource: "dnsmasq".to_string(),
            reason: format!(
                "no dnsmasq config for network '{}': ensure topology is created first",
                network_id
            ),
        });
    }

    // Build DNS lines to inject
    let mut dns_lines: Vec<String> = Vec::new();

    // Enable DNS port (override port=0 which disables DNS)
    dns_lines.push("port=53".to_string());

    // Add forwarders
    for fwd in forwarders {
        if fwd.parse::<std::net::IpAddr>().is_err() {
            return Err(ChvError::InvalidArgument {
                field: "forwarder".to_string(),
                reason: format!("invalid forwarder IP: '{}'", fwd),
            });
        }
        dns_lines.push(format!("server={}", fwd));
    }

    // Add static records (hostname -> IP)
    for (hostname, ip) in static_records {
        if !is_valid_hostname(hostname) {
            return Err(ChvError::InvalidArgument {
                field: "static_records".to_string(),
                reason: format!(
                    "invalid hostname '{}': must be RFC 1123 compliant (alphanumeric, hyphens, max 253 chars)",
                    hostname
                ),
            });
        }
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err(ChvError::InvalidArgument {
                field: "static_records".to_string(),
                reason: format!("invalid IP for record '{}': '{}'", hostname, ip),
            });
        }
        dns_lines.push(format!("address=/{}/{}", hostname, ip));
    }

    // Rebuild config: take existing DHCP lines, replace DNS lines
    let mut new_config_lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        // Skip old DNS-specific lines we're replacing
        if line.starts_with("port=")
            || line.starts_with("server=")
            || line.starts_with("address=/")
            || line == "no-resolv"
        {
            continue;
        }
        new_config_lines.push(line.to_string());
    }

    // Add DNS config
    new_config_lines.push("no-resolv".to_string());
    new_config_lines.extend(dns_lines);

    let new_config = new_config_lines.join("\n") + "\n";

    let _ = tokio::fs::create_dir_all(RUNTIME_DIR).await;
    tokio::fs::write(&conf, &new_config)
        .await
        .map_err(|e| ChvError::Io {
            path: conf.to_string_lossy().to_string(),
            source: e,
        })?;

    if is_dnsmasq_running(network_id).await {
        reload_dnsmasq(network_id).await;
        info!(network_id = %network_id, forwarder_count = forwarders.len(), "DNS scope updated, dnsmasq reloaded");
    } else {
        // Start dnsmasq with the updated config. The lease file must be
        // explicit: dnsmasq's default (/var/lib/misc) is not writable in the
        // nwd sandbox, and a denied lease file kills the daemon (rc.6 B4).
        let pid = pid_path(network_id);
        let lease = crate::dhcp::lease_path(network_id);
        let out = Command::new("dnsmasq")
            .args([
                &format!("--conf-file={}", conf.display()),
                &format!("--pid-file={}", pid.display()),
                &format!("--dhcp-leasefile={}", lease.display()),
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
        info!(network_id = %network_id, forwarder_count = forwarders.len(), "DNS scope applied, dnsmasq started");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conf_path() {
        let path = conf_path("net-1");
        assert_eq!(path.to_str().unwrap(), "/run/chv/nwd/dnsmasq-net-1.conf");
    }

    #[test]
    fn test_pid_path() {
        let path = pid_path("net-1");
        assert_eq!(path.to_str().unwrap(), "/run/chv/nwd/dnsmasq-net-1.pid");
    }
}
