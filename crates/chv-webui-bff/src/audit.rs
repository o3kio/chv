//! Durable audit trail for privileged BFF mutations (#658).
//!
//! Emission is centralized here, in middleware wrapped around the
//! whole HTTP surface that carries privileged mutations. Individual
//! handlers do NOT remember to call an audit function: the middleware
//! classifies the request by method and path, runs it, and writes
//! exactly one row with the final HTTP status. Because it wraps the
//! role middlewares too, a 401/403 denial is recorded with the
//! credential's real actor (or the explicit anonymous marker when no
//! credential validated).
//!
//! # Audited surfaces
//!
//! The classifier covers every non-safe method under:
//!
//! - `/v1/*` — the BFF surface, including the guest-agent browser
//!   routes (`/v1/monitoring/agents/*`, mounted by the control plane
//!   outside the BFF router) and `/v1/auth/change-password` /
//!   `/v1/auth/logout` (self-service but security-relevant);
//! - `/admin/*` — control-plane admin mutations;
//! - `/api/v1/*` — the legacy cookie-authenticated surface;
//! - `/internal/*` — control-plane internal management.
//!
//! Excluded: `POST /v1/auth/login` and `POST /api/v1/auth/login`
//! (credential entry — the body carries the password, the caller is
//! anonymous by definition, and brute-force volume is accounted for by
//! auth rate limiting, not by this trail) and the machine-to-machine
//! guest ingest transport `/monitoring/v1/*` (TLS-certificate
//! authenticated, high frequency, not a privileged human mutation).
//! Unknown non-safe paths on an audited surface fail closed into a
//! row (probing leaves a trace).
//!
//! # Mounting
//!
//! Standalone callers get all of this from
//! [`crate::router::bff_router`]. The production control plane mounts
//! [`crate::router::bff_router_unaudited`] inside
//! `admin_router_with_guest_agents` and applies this middleware at the
//! outermost layer there, so the extra privileged surfaces and the
//! router fallback are covered too — and nothing is audited twice.
//!
//! Redaction contract: the BFF never reads a request body, query string,
//! Authorization header, Cookie header, password, token, or S3 key into
//! the audit row. `metadata` is built from a tiny allowlist (the auth
//! mechanism classification only). The durable store's insert API takes
//! the already-redacted JSON object.

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chv_controlplane_store::{
    AuditLogInsert, AuditLogRepository, AUDIT_ANONYMOUS_ACTOR_ID, AUDIT_ANONYMOUS_ROLE,
    AUDIT_ANONYMOUS_USERNAME, AUDIT_OUTCOME_DENIED, AUDIT_OUTCOME_FAILED, AUDIT_OUTCOME_SUCCESS,
};
use serde_json::json;

use crate::auth::{authenticate_headers, AuthSource};
use crate::router::AppState;

/// Legacy tracing-only emitter retained for existing handler call sites.
/// The durable row is written by [`record_privileged_mutation`]; this
/// function intentionally contains no database access and never carries
/// request bodies or credentials.
pub fn log_mutation(user: &str, action: &str, resource_kind: &str, resource_id: &str) {
    tracing::info!(
        target: "audit",
        user = %user,
        action = %action,
        resource_kind = %resource_kind,
        resource_id = %resource_id,
        "mutation"
    );
}

/// Maximum stored length for client-supplied identifier strings
/// (`x-operation-id`, the correlation id). Identifiers are telemetry,
/// not data; capping them bounds row size against hostile or buggy
/// clients that send megabyte header values.
const MAX_CLIENT_ID_LEN: usize = 128;

fn capped_client_id(value: String) -> String {
    value.chars().take(MAX_CLIENT_ID_LEN).collect()
}

/// Maximum stored length for path-derived fields (`path`, `action`,
/// `resource_kind`, `resource_id`). These strings can embed
/// attacker-chosen URL segments (including on the fail-closed unknown
/// path arm); capping them bounds row size without affecting routing,
/// filtering on normal values, or the action/resource vocabulary.
const MAX_AUDIT_FIELD_LEN: usize = 256;

fn capped_field(value: String) -> String {
    value.chars().take(MAX_AUDIT_FIELD_LEN).collect()
}

/// Middleware: append exactly one durable audit row for every mutating
/// request on an audited surface, then return the handler/role-middleware
/// response.
pub async fn record_privileged_mutation(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let Some(classification) = classify_mutation(&method, &path) else {
        return next.run(req).await;
    };

    let correlation_id = req
        .extensions()
        .get::<Option<String>>()
        .and_then(|id| id.clone())
        .or_else(|| header_value(&req, "x-correlation-id"))
        .map(capped_client_id);
    let operation_id = header_value(&req, "x-operation-id").map(capped_client_id);
    let source_ip = extract_source_ip(&req);

    // Decode the same credential the authorization layer uses. On any
    // failure the actor is explicitly anonymous — never inferred from an
    // unverified header or body field. The decoded result is also shared
    // with the `BearerToken` extractor via extensions so authorization
    // and audit attribution cannot drift (#658 review L6).
    let decoded = authenticate_headers(req.headers(), &state).await;
    let (actor, auth_source) = match &decoded {
        Ok((claims, source)) => (claims.clone(), *source),
        Err(_) => (
            crate::auth::Claims {
                sub: AUDIT_ANONYMOUS_ACTOR_ID.to_string(),
                username: AUDIT_ANONYMOUS_USERNAME.to_string(),
                role: AUDIT_ANONYMOUS_ROLE.to_string(),
                exp: 0,
                must_change_password: false,
                aud: None,
                token_version: None,
            },
            AuthSource::Anonymous,
        ),
    };

    let mut req = req;
    req.extensions_mut()
        .insert(crate::auth::VerifiedAuth(decoded));

    let response = next.run(req).await;
    let status = response.status();
    let outcome = outcome_for_status(status);
    let status_code = i64::from(status.as_u16());

    // The metadata object is an explicit allowlist. Do not add request
    // headers, bodies, query strings, or handler payloads here.
    let metadata = json!({ "auth_source": auth_source.as_str() }).to_string();

    let entry = AuditLogInsert {
        occurred_at_ms: chv_common::now_unix_ms(),
        actor_id: actor.sub,
        actor_username: actor.username,
        actor_role: actor.role,
        action: capped_field(classification.action),
        method: method.as_str().to_string(),
        path: capped_field(path),
        resource_kind: classification.resource_kind.map(capped_field),
        resource_id: classification.resource_id.map(capped_field),
        outcome: outcome.to_string(),
        status_code,
        correlation_id,
        operation_id,
        source_ip,
        metadata,
    };

    match AuditLogRepository::new(state.pool.clone())
        .insert(&entry)
        .await
    {
        Ok(audit_id) => {
            tracing::debug!(
                target: "audit",
                audit_id = %audit_id,
                status = status_code,
                outcome = outcome,
                "audit row recorded"
            );
            response
        }
        Err(error) => {
            // Loud, structured, and never silent: an audit write failure
            // after a mutation succeeded must not be reported as success.
            // The dedicated counter makes a systemic audit-store outage
            // visible on dashboards even though the metrics middleware
            // records the final status on its own (#658 review M3).
            metrics::counter!("chv_bff_audit_write_failures_total").increment(1);
            tracing::error!(
                target: "audit",
                error = %error,
                action = %entry.action,
                method = %entry.method,
                path = %entry.path,
                status = status_code,
                outcome = outcome,
                "AUDIT LOG WRITE FAILED — privileged mutation may not be durably recorded"
            );
            if status.is_success() || status.is_redirection() {
                let mut body = json!({
                    "message": "privileged mutation completed but its audit record could not be persisted",
                    "code": "AUDIT_LOG_WRITE_FAILED"
                });
                // Explicit correlation handle so the operator can find
                // what the mutation actually did (#658 review M3). The
                // outer correlation middleware re-inserts the same id.
                if let Some(id) = &entry.correlation_id {
                    body["request_id"] = json!(id);
                }
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            } else {
                // Preserve the original denial/failure: replacing it could
                // hide the real error from the caller, and the loud log
                // above already records the missing audit row.
                response
            }
        }
    }
}

fn outcome_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        AUDIT_OUTCOME_DENIED
    } else if status.is_success() || status.is_redirection() {
        AUDIT_OUTCOME_SUCCESS
    } else {
        AUDIT_OUTCOME_FAILED
    }
}

fn header_value(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Source IP is advisory telemetry, not an authorization input. Prefer
/// the transport peer address when the server provides connect info
/// (both the plain-HTTP and TLS serve paths inject it); otherwise
/// accept the first valid `x-forwarded-for` / `x-real-ip` hop — a
/// value only a **trusted reverse proxy** can be expected to set
/// correctly. Without ConnectInfo, a direct client can spoof these
/// headers; deployments that do not sit behind such a proxy must treat
/// `source_ip` as unverified. Invalid values are dropped rather than
/// stored verbatim.
fn extract_source_ip(req: &Request) -> Option<String> {
    if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return Some(addr.ip().to_string());
    }
    if let Some(forwarded) = header_value(req, "x-forwarded-for") {
        if let Some(first) = forwarded.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return Some(ip.to_string());
            }
        }
    }
    header_value(req, "x-real-ip")
        .and_then(|value| value.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct MutationClassification {
    action: String,
    resource_kind: Option<String>,
    resource_id: Option<String>,
}

/// Method/path classifier. Any non-safe method on an audited surface
/// is audited unless it is explicitly a read-shaped POST. Audited
/// surfaces: `/v1/*` (including the guest-agent browser routes and
/// the self-service `change-password`/`logout`), `/admin/*`,
/// `/api/v1/*` (legacy), and `/internal/*`. Excluded: the login
/// credential-entry endpoints (bodies carry passwords; anonymous
/// brute-force volume belongs to auth rate limiting, not this trail)
/// and the machine-to-machine guest ingest transport
/// `/monitoring/v1/*`. The fallback treats an unknown non-safe path
/// on an audited surface as a mutation (fail-closed: a new write
/// route — or mere probing of an unrouted one — is audited by
/// default, never silently omitted).
fn classify_mutation(method: &Method, path: &str) -> Option<MutationClassification> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    // Machine-to-machine guest ingest transport: TLS-certificate
    // authenticated, high frequency, not a privileged human mutation.
    if path.starts_with("/monitoring/v1/") {
        return None;
    }
    // The legacy /api/v1 surface shares the /v1 classification table;
    // the row still stores the original request path.
    let normalized;
    let path = if let Some(rest) = path.strip_prefix("/api/v1") {
        normalized = format!("/v1{rest}");
        normalized.as_str()
    } else {
        path
    };
    if !path.starts_with("/v1/") && !path.starts_with("/admin/") && !path.starts_with("/internal/")
    {
        return None;
    }
    // Credential entry: unauthenticated by definition, password in the
    // body, and brute-force volume — noise for this trail.
    if path == "/v1/auth/login" {
        return None;
    }
    if is_read_shaped_post(method, path) {
        return None;
    }

    let (action, resource_kind) = action_and_kind(method, path);
    Some(MutationClassification {
        action,
        resource_kind: Some(resource_kind.to_string()),
        resource_id: resource_id_from_path(path),
    })
}

fn is_read_shaped_post(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }

    if path == "/v1/quotas/check" {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/v1/quotas/") {
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() == 2 && segments[1] == "usage" {
            return true;
        }
    }
    // Rendering a stored cloud-init template substitutes variables and
    // returns text; it writes nothing (and is deliberately not a
    // mutation of the template).
    if let Some(rest) = path.strip_prefix("/v1/cloud-init-templates/") {
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() == 2 && segments[1] == "render" {
            return true;
        }
    }

    matches!(
        path,
        "/v1/overview"
            | "/v1/metrics"
            | "/v1/monitoring/catalog"
            | "/v1/monitoring/overview"
            | "/v1/monitoring/current"
            | "/v1/monitoring/checks"
            | "/v1/monitoring/history"
            | "/v1/monitoring/alerts"
            | "/v1/monitoring/alerts/detail"
            | "/v1/monitoring/alert-rules"
            | "/v1/monitoring/notifications/deliveries"
            | "/v1/monitoring/agents"
            | "/v1/nodes"
            | "/v1/nodes/get"
            | "/v1/vms"
            | "/v1/vms/get"
            | "/v1/vms/console"
            | "/v1/vms/events"
            | "/v1/vms/snapshots"
            | "/v1/tasks"
            | "/v1/tasks/get"
            | "/v1/clusters"
            | "/v1/networks"
            | "/v1/networks/get"
            | "/v1/events"
            | "/v1/images"
            | "/v1/maintenance"
            | "/v1/settings"
            | "/v1/settings/hypervisor"
            | "/v1/settings/hypervisor/profiles"
            | "/v1/volumes"
            | "/v1/volumes/get"
            | "/v1/quotas"
            | "/v1/quotas/me"
            | "/v1/usage"
            | "/v1/tokens"
            | "/v1/users"
            | "/v1/audit-log"
            | "/v1/audit-log/export"
            | "/v1/architectures/list"
            | "/v1/architectures/get"
            | "/v1/architectures/validate"
            | "/v1/architectures/versions/list"
            | "/v1/architectures/check-fleet"
            | "/v1/architectures/generate-yaml"
            | "/v1/architectures/validate-yaml"
            | "/v1/architectures/runs/list"
            | "/v1/architectures/netbox/config/get"
            | "/v1/architectures/netbox/export/dry-run"
            | "/v1/architectures/netbox/runs/list"
            | "/v1/architectures/netbox/runs/get"
    )
}

fn action_and_kind(method: &Method, path: &str) -> (String, &'static str) {
    let (action, kind) = match path {
        "/v1/auth/change-password" => ("auth.change_password", "user"),
        "/v1/auth/logout" => ("auth.logout", "user"),
        "/v1/nodes/mutate" => ("node.mutate", "node"),
        "/v1/nodes/enroll" => ("node.enroll", "node"),
        "/v1/monitoring/alert-rules/create" => ("alert_rule.create", "alert_rule"),
        "/v1/monitoring/alert-rules/update" => ("alert_rule.update", "alert_rule"),
        "/v1/monitoring/alert-rules/delete" => ("alert_rule.delete", "alert_rule"),
        "/v1/monitoring/alerts/acknowledge" => ("alert.acknowledge", "incident"),
        "/v1/monitoring/alerts/silence" => ("alert.silence", "incident"),
        "/v1/monitoring/notifications/test" => ("notification.test", "notification"),
        // Guest-agent browser surface (operator tier, mounted by the
        // control plane outside the BFF router — audited since the
        // review-round restructure).
        "/v1/monitoring/agents/claim" => ("agent.issue_claim", "monitoring_agent"),
        "/v1/monitoring/agents/revoke" => ("agent.revoke", "monitoring_agent"),
        "/v1/monitoring/agents/rotate" => ("agent.rotate", "monitoring_agent"),
        "/v1/monitoring/agents/reset" => ("agent.reset", "monitoring_agent"),
        "/v1/vms/create" => ("vm.create", "vm"),
        "/v1/vms/delete" => ("vm.delete", "vm"),
        "/v1/vms/resize" => ("vm.resize", "vm"),
        "/v1/vms/mutate" => ("vm.mutate", "vm"),
        "/v1/vms/import" => ("vm.import", "vm"),
        "/v1/vms/snapshots/create" => ("snapshot.create", "snapshot"),
        "/v1/vms/snapshots/delete" => ("snapshot.delete", "snapshot"),
        "/v1/vms/snapshots/restore" => ("snapshot.restore", "snapshot"),
        "/v1/networks/create" => ("network.create", "network"),
        "/v1/networks/delete" => ("network.delete", "network"),
        "/v1/networks/update" => ("network.update", "network"),
        "/v1/networks/mutate" => ("network.mutate", "network"),
        "/v1/images/import" => ("image.import", "image"),
        "/v1/images/delete" => ("image.delete", "image"),
        "/v1/volumes/create" => ("volume.create", "volume"),
        "/v1/volumes/mutate" => ("volume.mutate", "volume"),
        "/v1/volumes/snapshot" => ("volume.snapshot", "volume"),
        "/v1/volumes/restore-snapshot" => ("volume.restore_snapshot", "volume"),
        "/v1/volumes/delete-snapshot" => ("volume.delete_snapshot", "volume"),
        "/v1/volumes/delete" => ("volume.delete", "volume"),
        "/v1/volumes/clone" => ("volume.clone", "volume"),
        "/v1/vm-templates" => ("vm_template.create", "vm_template"),
        "/v1/cloud-init-templates" => ("cloud_init_template.create", "cloud_init_template"),
        "/v1/tokens/create" => ("api_token.create", "api_token"),
        "/v1/tokens/revoke" => ("api_token.revoke", "api_token"),
        "/v1/backup-jobs" => ("backup.create", "backup"),
        "/v1/backup-history" => ("backup.list_attempt", "backup"),
        "/v1/backups/jobs" => ("backup.create", "backup"),
        "/v1/backups/schedules" => ("backup_schedule.create", "backup_schedule"),
        "/v1/backups/restores" => ("backup_restore.create", "backup_restore"),
        "/v1/architectures/create" => ("architecture.create", "architecture"),
        "/v1/architectures/update" => ("architecture.update", "architecture"),
        "/v1/architectures/archive" => ("architecture.archive", "architecture"),
        "/v1/architectures/import-yaml" => ("architecture.import_yaml", "architecture"),
        "/v1/architectures/plan" => ("architecture.plan", "architecture"),
        "/v1/architectures/destroy-plan" => ("architecture.destroy_plan", "architecture"),
        "/v1/architectures/discard-plan" => ("architecture.discard_plan", "architecture"),
        "/v1/architectures/apply" => ("architecture.apply", "architecture"),
        "/v1/architectures/destroy" => ("architecture.destroy", "architecture"),
        "/v1/architectures/drift" => ("architecture.drift", "architecture"),
        "/v1/architectures/netbox/config/upsert" => ("netbox_config.upsert", "netbox_config"),
        "/v1/architectures/netbox/config/delete" => ("netbox_config.delete", "netbox_config"),
        "/v1/architectures/netbox/export" => ("netbox.export", "netbox_run"),
        "/v1/architectures/netbox/runs/retry" => ("netbox.retry", "netbox_run"),
        "/v1/settings/hypervisor/update" => ("hypervisor_settings.update", "hypervisor_settings"),
        "/v1/quotas/create" => ("quota.create", "quota"),
        "/v1/users/create" => ("user.create", "user"),
        "/v1/users/update" => ("user.update", "user"),
        "/v1/users/delete" => ("user.delete", "user"),
        _ => {
            // Dynamic/parameterized routes and future additions.
            if let Some(rest) = path.strip_prefix("/v1/vms/") {
                if rest.ends_with("/export") {
                    return ("vm.export".to_string(), "vm");
                }
            }
            // Legacy cookie-authenticated backup surface (reached via
            // the /api/v1 → /v1 normalization): run/toggle/delete a
            // fenced job by path id.
            if let Some(rest) = path.strip_prefix("/v1/backup-jobs/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() == 2 && segments[1] == "run" {
                    return ("backup.run".to_string(), "backup");
                }
                if segments.len() == 2 && segments[1] == "toggle" {
                    return ("backup.toggle".to_string(), "backup");
                }
                if *method == Method::DELETE {
                    return ("backup.delete".to_string(), "backup");
                }
            }
            // Control-plane admin mutations.
            if let Some(rest) = path.strip_prefix("/admin/migrations/") {
                if rest.ends_with("/cancel") {
                    return ("migration.cancel".to_string(), "migration");
                }
            }
            // Control-plane internal management.
            if path == "/internal/bootstrap-token" {
                return ("bootstrap_token.seed".to_string(), "bootstrap_token");
            }
            if path.starts_with("/v1/vm-templates/") {
                if path.ends_with("/clone") {
                    return ("vm_template.clone".to_string(), "vm_template");
                }
                if *method == Method::DELETE {
                    return ("vm_template.delete".to_string(), "vm_template");
                }
            }
            if path.starts_with("/v1/cloud-init-templates/") {
                if path.ends_with("/render") {
                    return (
                        "cloud_init_template.render".to_string(),
                        "cloud_init_template",
                    );
                }
                if *method == Method::DELETE {
                    return (
                        "cloud_init_template.delete".to_string(),
                        "cloud_init_template",
                    );
                }
            }
            if let Some(rest) = path.strip_prefix("/v1/backups/jobs/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() >= 2 && segments[1] == "execute" {
                    return ("backup.execute".to_string(), "backup");
                }
                if *method == Method::PATCH {
                    return ("backup.update".to_string(), "backup");
                }
                if *method == Method::DELETE {
                    return ("backup.delete".to_string(), "backup");
                }
            }
            if path.starts_with("/v1/backups/schedules/") {
                if *method == Method::PATCH {
                    return ("backup_schedule.update".to_string(), "backup_schedule");
                }
                if *method == Method::DELETE {
                    return ("backup_schedule.delete".to_string(), "backup_schedule");
                }
            }
            if path.starts_with("/v1/settings/hypervisor/apply-profile") {
                return (
                    "hypervisor_settings.apply".to_string(),
                    "hypervisor_settings",
                );
            }
            if path == "/v1/settings/hypervisor" && *method == Method::PATCH {
                return (
                    "hypervisor_settings.update".to_string(),
                    "hypervisor_settings",
                );
            }
            if let Some(rest) = path.strip_prefix("/v1/quotas/") {
                let segments: Vec<&str> = rest.split('/').collect();
                if segments.len() == 1 {
                    if *method == Method::PATCH {
                        return ("quota.update".to_string(), "quota");
                    }
                    if *method == Method::DELETE {
                        return ("quota.delete".to_string(), "quota");
                    }
                }
            }

            let kind = path
                .trim_start_matches('/')
                .split('/')
                .nth(1)
                .unwrap_or("unknown");
            let kind = match kind {
                "users" => "user",
                "tokens" => "api_token",
                "vms" => "vm",
                "vm-templates" => "vm_template",
                "cloud-init-templates" => "cloud_init_template",
                "networks" => "network",
                "volumes" => "volume",
                "images" => "image",
                "nodes" => "node",
                "quotas" => "quota",
                "architectures" => "architecture",
                "monitoring" => "monitoring",
                "settings" => "settings",
                "backups" | "backup-jobs" | "backup-history" => "backup",
                other => other,
            };
            return (
                format!("{kind}.{}", method.as_str().to_ascii_lowercase()),
                // Unknown future route: the action keeps the path-derived
                // noun for filtering, while the typed kind stays explicit.
                "unknown",
            );
        }
    };
    (action.to_string(), kind)
}

const PATH_ACTION_WORDS: &[&str] = &[
    "create",
    "delete",
    "update",
    "mutate",
    "resize",
    "import",
    "export",
    "snapshot",
    "restore-snapshot",
    "delete-snapshot",
    "clone",
    "list",
    "get",
    "validate",
    "versions",
    "plan",
    "apply",
    "destroy",
    "archive",
    "check-fleet",
    "generate-yaml",
    "validate-yaml",
    "import-yaml",
    "discard-plan",
    "destroy-plan",
    "runs",
    "drift",
    "config",
    "netbox",
    "overview",
    "history",
    "current",
    "checks",
    "health",
    "catalog",
    "alerts",
    "alert-rules",
    "notifications",
    "profiles",
    "apply-profile",
    "create",
    "revoke",
    "enroll",
    "execute",
    "snapshots",
];

fn resource_id_from_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let second = *segments.get(1)?;
    let third = *segments.get(2)?;

    match second {
        "backups" => {
            // /v1/backups/{jobs|schedules|restores}/:id[/execute]
            let id = *segments.get(3)?;
            if id.is_empty() || PATH_ACTION_WORDS.contains(&id) {
                None
            } else {
                Some(id.to_string())
            }
        }
        "settings" => {
            // /v1/settings/hypervisor/apply-profile/:id
            if segments.get(3) == Some(&"apply-profile") {
                segments
                    .get(4)
                    .filter(|id| !id.is_empty())
                    .map(|id| id.to_string())
            } else {
                None
            }
        }
        "quotas" => {
            if third == "check"
                || third == "me"
                || segments.get(3) == Some(&"usage")
                || PATH_ACTION_WORDS.contains(&third)
            {
                None
            } else {
                Some(third.to_string())
            }
        }
        _ => {
            if PATH_ACTION_WORDS.contains(&third) {
                None
            } else {
                Some(third.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_requests_are_not_mutations() {
        assert!(classify_mutation(&Method::GET, "/v1/vms").is_none());
        assert!(classify_mutation(&Method::GET, "/v1/audit-log").is_none());
    }

    #[test]
    fn read_shaped_posts_are_not_mutations() {
        assert!(classify_mutation(&Method::POST, "/v1/vms").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/vms/get").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/audit-log").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/users").is_none());
        assert!(classify_mutation(&Method::POST, "/v1/quotas/u-1/usage").is_none());
        // Guest-agent inventory list (viewer tier) is a read.
        assert!(classify_mutation(&Method::POST, "/v1/monitoring/agents").is_none());
        // Cloud-init template rendering is a pure read (no write, no task).
        assert!(classify_mutation(&Method::POST, "/v1/cloud-init-templates/t-1/render").is_none());
        // A render-shaped suffix on another path is not read-shaped.
        assert!(
            classify_mutation(&Method::POST, "/v1/cloud-init-templates/t-1/render/extra").is_some()
        );
    }

    #[test]
    fn operator_and_admin_mutations_are_classified() {
        let class = classify_mutation(&Method::POST, "/v1/vms/create").expect("mutation");
        assert_eq!(class.action, "vm.create");
        assert_eq!(class.resource_kind.as_deref(), Some("vm"));

        let class = classify_mutation(&Method::DELETE, "/v1/vm-templates/t-1").expect("mutation");
        assert_eq!(class.action, "vm_template.delete");
        assert_eq!(class.resource_id.as_deref(), Some("t-1"));

        let class = classify_mutation(&Method::PATCH, "/v1/quotas/u-1").expect("mutation");
        assert_eq!(class.action, "quota.update");
        assert_eq!(class.resource_id.as_deref(), Some("u-1"));
    }

    #[test]
    fn unknown_non_safe_v1_paths_fail_closed_to_mutation() {
        let class = classify_mutation(&Method::POST, "/v1/future-write").expect("mutation");
        assert_eq!(class.action, "future-write.post");
    }

    #[test]
    fn login_is_excluded_but_security_relevant_auth_routes_are_audited() {
        // Login is credential entry: password in the body, anonymous
        // caller, brute-force volume — deliberately outside the trail.
        assert!(classify_mutation(&Method::POST, "/v1/auth/login").is_none());
        assert!(classify_mutation(&Method::POST, "/api/v1/auth/login").is_none());
        // change-password and logout are security-relevant self-service
        // actions (a session thief's seizure step) and ARE recorded —
        // with actor/action only, never the body.
        let class = classify_mutation(&Method::POST, "/v1/auth/change-password").expect("mutation");
        assert_eq!(class.action, "auth.change_password");
        assert_eq!(class.resource_kind.as_deref(), Some("user"));
        let class = classify_mutation(&Method::POST, "/v1/auth/logout").expect("mutation");
        assert_eq!(class.action, "auth.logout");
        // The legacy /api/v1 logout is classified identically.
        let class = classify_mutation(&Method::POST, "/api/v1/auth/logout").expect("mutation");
        assert_eq!(class.action, "auth.logout");
    }

    #[test]
    fn control_plane_admin_internal_and_legacy_surfaces_are_audited() {
        let class =
            classify_mutation(&Method::POST, "/admin/migrations/m-1/cancel").expect("mutation");
        assert_eq!(class.action, "migration.cancel");
        assert_eq!(class.resource_kind.as_deref(), Some("migration"));
        assert_eq!(class.resource_id.as_deref(), Some("m-1"));

        let class =
            classify_mutation(&Method::POST, "/internal/bootstrap-token").expect("mutation");
        assert_eq!(class.action, "bootstrap_token.seed");
        assert_eq!(class.resource_kind.as_deref(), Some("bootstrap_token"));

        // Legacy /api/v1 backup surface (normalized onto /v1).
        let class =
            classify_mutation(&Method::POST, "/api/v1/backup-jobs/j-9/run").expect("mutation");
        assert_eq!(class.action, "backup.run");
        assert_eq!(class.resource_id.as_deref(), Some("j-9"));
        let class =
            classify_mutation(&Method::DELETE, "/api/v1/backup-jobs/j-9").expect("mutation");
        assert_eq!(class.action, "backup.delete");
        assert_eq!(class.resource_id.as_deref(), Some("j-9"));

        // Unknown non-safe paths on every audited surface fail closed.
        assert!(classify_mutation(&Method::POST, "/admin/future").is_some());
        assert!(classify_mutation(&Method::POST, "/internal/future").is_some());
        assert!(classify_mutation(&Method::POST, "/api/v1/future").is_some());
    }

    #[test]
    fn guest_agent_browser_mutations_are_audited_but_ingest_is_not() {
        for path in [
            "/v1/monitoring/agents/claim",
            "/v1/monitoring/agents/revoke",
            "/v1/monitoring/agents/rotate",
            "/v1/monitoring/agents/reset",
        ] {
            let class = classify_mutation(&Method::POST, path).expect("mutation");
            assert_eq!(class.resource_kind.as_deref(), Some("monitoring_agent"));
        }
        // The certificate-authenticated machine ingest transport is
        // excluded: high frequency, not a privileged human mutation.
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/enroll").is_none());
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/ingest").is_none());
        assert!(classify_mutation(&Method::POST, "/monitoring/v1/rotate").is_none());
        // Non-audited surfaces stay excluded.
        assert!(classify_mutation(&Method::POST, "/some/static/path").is_none());
        assert!(classify_mutation(&Method::POST, "/health").is_none());
    }

    #[test]
    fn client_identifiers_are_capped() {
        let long = "x".repeat(5_000);
        assert_eq!(capped_client_id(long.clone()).len(), MAX_CLIENT_ID_LEN);
        assert_eq!(capped_client_id("short".to_string()), "short");
    }

    #[test]
    fn path_derived_fields_are_capped() {
        let long = "x".repeat(5_000);
        assert_eq!(
            capped_field(long.clone()).chars().count(),
            MAX_AUDIT_FIELD_LEN
        );
        assert_eq!(capped_field("vm.create".to_string()), "vm.create");
        assert_eq!(
            capped_field("é".repeat(1_000)).chars().count(),
            MAX_AUDIT_FIELD_LEN,
            "cap counts characters, not bytes"
        );
    }

    #[test]
    fn denied_and_failed_status_mapping() {
        assert_eq!(outcome_for_status(StatusCode::OK), AUDIT_OUTCOME_SUCCESS);
        assert_eq!(
            outcome_for_status(StatusCode::FORBIDDEN),
            AUDIT_OUTCOME_DENIED
        );
        assert_eq!(
            outcome_for_status(StatusCode::UNAUTHORIZED),
            AUDIT_OUTCOME_DENIED
        );
        assert_eq!(
            outcome_for_status(StatusCode::BAD_REQUEST),
            AUDIT_OUTCOME_FAILED
        );
        assert_eq!(
            outcome_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            AUDIT_OUTCOME_FAILED
        );
    }
}
