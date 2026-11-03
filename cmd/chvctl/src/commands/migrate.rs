use clap::Subcommand;
use serde_json::json;

use crate::client::{BffClient, CliError};
use crate::output::{self, OutputFormat};

#[derive(Subcommand)]
pub enum MigrateCommands {
    /// Start a stop-the-world migration for a VM
    Start {
        /// VM identifier to migrate
        vm_id: String,
        /// Target node to migrate to
        target_node: String,
        /// Required: pause the VM for the full transfer (stop-the-world).
        /// Live migration is disabled until #394 is proven; omitting this
        /// flag fails closed without sending a request.
        #[arg(long)]
        pause_first: bool,
    },
    /// Check status of a migration
    Status {
        /// Migration identifier
        migration_id: String,
    },
    /// Cancel an in-progress migration (admin role required)
    Cancel {
        /// Migration identifier
        migration_id: String,
    },
    /// List all migrations
    List,
}

pub async fn execute(
    client: &BffClient,
    command: MigrateCommands,
    format: &OutputFormat,
) -> Result<(), CliError> {
    match command {
        MigrateCommands::Start {
            vm_id,
            target_node,
            pause_first,
        } => {
            crate::commands::require_pause_first(pause_first)?;
            // #372 DP4: there is no POST /v1/migrations route — the real
            // entry point is the vm-mutate migrate action, the exact path
            // `chvctl vm migrate` drives. The wire field is
            // `target_node_id`; the old body's `target_node` key was read
            // by nothing (the route it targeted did not exist).
            let body = json!({
                "vm_id": vm_id,
                "action": "migrate",
                "target_node_id": target_node,
                "pause_first": pause_first,
            });
            let resp = client.post("/v1/vms/mutate", &body).await?;
            println!("Migration initiated for VM {vm_id} to node {target_node}.");
            output::print_value(&resp, format);
        }
        MigrateCommands::Status { migration_id } => {
            // #372 DP4b: the viewer-tier read route this command always
            // targeted — it 404'd from introduction until the route
            // existed. Unknown ids are a 404.
            let resp = client
                .get(&format!("/v1/migrations/{}", migration_id))
                .await?;
            output::print_value(&resp, format);
        }
        MigrateCommands::Cancel { migration_id } => {
            // #372 DP4: the real cancel route is the control plane's
            // admin-tier POST /admin/migrations/{id}/cancel — admin role
            // required (an operator-role token gets a 403), and the
            // server must be a CP admin endpoint, not a plain BFF bind.
            // The cancel is cooperative and best-effort: the migration
            // loop observes the flag at a safe point and rolls back; the
            // response's `outcome` says whether this call set the flag
            // (`requested`) or was a no-op (`already_requested`,
            // `already_terminal`). The route takes no body.
            let resp = client
                .post(
                    &format!("/admin/migrations/{}/cancel", migration_id),
                    &json!({}),
                )
                .await?;
            println!("Migration {migration_id} cancel requested.");
            output::print_value(&resp, format);
        }
        MigrateCommands::List => {
            // #372 DP4b: the viewer-tier list route; the items carry the
            // migrations table's own column names (the old code read a
            // `migrations` key the BFF never served and printed columns
            // — `source_node`, `target_node`, `status`, `progress` —
            // that existed nowhere).
            let resp = client.get("/v1/migrations").await?;
            let items = resp
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            output::print_list(
                &items,
                &[
                    "migration_id",
                    "vm_id",
                    "source_node_id",
                    "destination_node_id",
                    "phase",
                    "cancel_requested",
                ],
                format,
            );
        }
    }
    Ok(())
}
