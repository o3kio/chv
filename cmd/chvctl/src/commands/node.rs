use clap::Subcommand;
use serde_json::json;

use crate::client::{BffClient, CliError};
use crate::output::{self, OutputFormat};

#[derive(Subcommand)]
pub enum NodeCommands {
    /// List all compute nodes
    List,
    /// Get details of a specific node
    Get {
        /// Node identifier
        node_id: String,
    },
    /// Drain a node (evacuate all VMs)
    Drain {
        /// Node identifier
        node_id: String,
    },
    /// Put a node into maintenance mode
    Maintenance {
        /// Node identifier
        node_id: String,
        /// Enable or disable maintenance mode
        #[arg(long)]
        enable: bool,
    },
    /// Revoke a node certificate (admin; denylist, checked at mTLS handshake)
    RevokeCertificate {
        /// Node identifier
        node_id: String,
        /// Certificate serial to revoke (defaults to the node's active serial)
        #[arg(long)]
        certificate_serial: Option<String>,
        /// Why the certificate is being revoked (recorded in the audit trail)
        #[arg(long)]
        reason: String,
        /// Optional RFC 3339 timestamp for pruning this denylist row later
        #[arg(long)]
        expires_at: Option<String>,
    },
}

pub async fn execute(
    client: &BffClient,
    command: NodeCommands,
    format: &OutputFormat,
) -> Result<(), CliError> {
    match command {
        NodeCommands::List => {
            let resp = client.post("/v1/nodes", &json!({})).await?;
            let items = resp
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            output::print_list(
                &items,
                &["node_id", "name", "state", "health", "cpu", "memory"],
                format,
            );
        }
        NodeCommands::Get { node_id } => {
            let resp = client
                .post("/v1/nodes/get", &json!({ "node_id": node_id }))
                .await?;
            output::print_value(&resp, format);
        }
        NodeCommands::Drain { node_id } => {
            let body = json!({ "node_id": node_id, "action": "drain" });
            client.post("/v1/nodes/mutate", &body).await?;
            println!("Node {node_id} draining.");
        }
        NodeCommands::Maintenance { node_id, enable } => {
            let action = if enable {
                "enter_maintenance"
            } else {
                "exit_maintenance"
            };
            let body = json!({ "node_id": node_id, "action": action });
            client.post("/v1/nodes/mutate", &body).await?;
            if enable {
                println!("Node {node_id} entering maintenance mode.");
            } else {
                println!("Node {node_id} exiting maintenance mode.");
            }
        }
        NodeCommands::RevokeCertificate {
            node_id,
            certificate_serial,
            reason,
            expires_at,
        } => {
            let mut body = json!({ "node_id": node_id, "reason": reason });
            if let Some(serial) = certificate_serial {
                body["certificate_serial"] = json!(serial);
            }
            if let Some(expires_at) = expires_at {
                body["expires_at"] = json!(expires_at);
            }
            let resp = client.post("/v1/nodes/revoke-certificate", &body).await?;
            let serial = resp
                .get("certificate_serial")
                .and_then(|value| value.as_str())
                .unwrap_or("<unknown>");
            let already = resp
                .get("already_revoked")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if already {
                println!("Certificate serial {serial} for node {node_id} was already revoked.");
            } else {
                println!(
                    "Revoked certificate serial {serial} for node {node_id}. \
                     The node must re-enroll with a fresh bootstrap token before it can \
                     authenticate or rotate again."
                );
            }
        }
    }
    Ok(())
}
