use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

// ------------------------------------------------------------------
// Local types & validation (uses chv_common::hypervisor)
// ------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct HypervisorSettingsPatchInput {
    pub cpu_nested: Option<bool>,
    pub cpu_amx: Option<bool>,
    pub cpu_kvm_hyperv: Option<bool>,
    pub memory_mergeable: Option<bool>,
    pub memory_hugepages: Option<bool>,
    pub memory_shared: Option<bool>,
    pub memory_prefault: Option<bool>,
    pub iommu: Option<bool>,
    pub rng_src: Option<String>,
    pub watchdog: Option<bool>,
    pub landlock_enable: Option<bool>,
    pub serial_mode: Option<String>,
    pub console_mode: Option<String>,
    pub pvpanic: Option<bool>,
    pub tpm_type: Option<String>,
    pub tpm_socket_path: Option<String>,
    pub profile_id: Option<String>,
}

fn validate_settings_patch(input: &HypervisorSettingsPatchInput) -> Result<(), String> {
    if let Some(ref mode) = input.serial_mode {
        chv_common::hypervisor::validate_serial_mode(mode)
            .map_err(|e| format!("serial_mode: {}", e))?;
    }
    if let Some(ref mode) = input.console_mode {
        chv_common::hypervisor::validate_console_mode(mode)
            .map_err(|e| format!("console_mode: {}", e))?;
    }
    if let Some(ref t) = input.tpm_type {
        chv_common::hypervisor::validate_tpm_type(t).map_err(|e| format!("tpm_type: {}", e))?;
    }
    if input.tpm_type.is_none() && input.tpm_socket_path.is_some() {
        return Err("tpm_socket_path requires tpm_type to be set".into());
    }
    if let Some(ref src) = input.rng_src {
        chv_common::hypervisor::validate_rng_src(src).map_err(|e| format!("rng_src: {}", e))?;
    }
    if let Some(ref path) = input.tpm_socket_path {
        if !path.starts_with('/') {
            return Err("tpm_socket_path must be an absolute path".into());
        }
    }
    if input.iommu == Some(true) && input.memory_shared != Some(true) {
        return Err("iommu=true requires memory_shared=true".into());
    }
    Ok(())
}

pub fn validate_vm_overrides(
    overrides: &chv_common::hypervisor::HypervisorOverrides,
) -> Result<(), String> {
    if let Some(ref mode) = overrides.serial_mode {
        chv_common::hypervisor::validate_serial_mode(mode)
            .map_err(|e| format!("serial_mode: {}", e))?;
    }
    if let Some(ref mode) = overrides.console_mode {
        chv_common::hypervisor::validate_console_mode(mode)
            .map_err(|e| format!("console_mode: {}", e))?;
    }
    if let Some(ref t) = overrides.tpm_type {
        chv_common::hypervisor::validate_tpm_type(t).map_err(|e| format!("tpm_type: {}", e))?;
    }
    if overrides.tpm_type.is_none() && overrides.tpm_socket_path.is_some() {
        return Err("tpm_socket_path requires tpm_type to be set".into());
    }
    if let Some(ref src) = overrides.rng_src {
        chv_common::hypervisor::validate_rng_src(src).map_err(|e| format!("rng_src: {}", e))?;
    }
    if let Some(ref path) = overrides.tpm_socket_path {
        if !path.starts_with('/') {
            return Err("tpm_socket_path must be an absolute path".into());
        }
    }
    if overrides.iommu == Some(true) && overrides.memory_shared != Some(true) {
        return Err("iommu=true requires memory_shared=true".into());
    }
    Ok(())
}

// ------------------------------------------------------------------
// Row types for raw SQL
// ------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct SettingsRow {
    cpu_nested: bool,
    cpu_amx: bool,
    cpu_kvm_hyperv: bool,
    memory_mergeable: bool,
    memory_hugepages: bool,
    memory_shared: bool,
    memory_prefault: bool,
    iommu: bool,
    rng_src: String,
    watchdog: bool,
    landlock_enable: bool,
    serial_mode: String,
    console_mode: String,
    pvpanic: bool,
    tpm_type: Option<String>,
    tpm_socket_path: Option<String>,
    profile_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ProfileRow {
    id: String,
    name: String,
    description: Option<String>,
    cpu_nested: Option<bool>,
    cpu_amx: Option<bool>,
    cpu_kvm_hyperv: Option<bool>,
    memory_mergeable: Option<bool>,
    memory_hugepages: Option<bool>,
    memory_shared: Option<bool>,
    memory_prefault: Option<bool>,
    iommu: Option<bool>,
    rng_src: Option<String>,
    watchdog: Option<bool>,
    landlock_enable: Option<bool>,
    serial_mode: Option<String>,
    console_mode: Option<String>,
    pvpanic: Option<bool>,
    tpm_type: Option<String>,
    tpm_socket_path: Option<String>,
    is_builtin: bool,
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

fn settings_to_json(row: SettingsRow) -> Value {
    json!({
        "cpu_nested": row.cpu_nested,
        "cpu_amx": row.cpu_amx,
        "cpu_kvm_hyperv": row.cpu_kvm_hyperv,
        "memory_mergeable": row.memory_mergeable,
        "memory_hugepages": row.memory_hugepages,
        "memory_shared": row.memory_shared,
        "memory_prefault": row.memory_prefault,
        "iommu": row.iommu,
        "rng_src": row.rng_src,
        "watchdog": row.watchdog,
        "landlock_enable": row.landlock_enable,
        "serial_mode": row.serial_mode,
        "console_mode": row.console_mode,
        "pvpanic": row.pvpanic,
        "tpm_type": row.tpm_type,
        "tpm_socket_path": row.tpm_socket_path,
        "profile_id": row.profile_id,
    })
}

fn profile_to_json(row: ProfileRow) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "description": row.description,
        "cpu_nested": row.cpu_nested,
        "cpu_amx": row.cpu_amx,
        "cpu_kvm_hyperv": row.cpu_kvm_hyperv,
        "memory_mergeable": row.memory_mergeable,
        "memory_hugepages": row.memory_hugepages,
        "memory_shared": row.memory_shared,
        "memory_prefault": row.memory_prefault,
        "iommu": row.iommu,
        "rng_src": row.rng_src,
        "watchdog": row.watchdog,
        "landlock_enable": row.landlock_enable,
        "serial_mode": row.serial_mode,
        "console_mode": row.console_mode,
        "pvpanic": row.pvpanic,
        "tpm_type": row.tpm_type,
        "tpm_socket_path": row.tpm_socket_path,
        "is_builtin": row.is_builtin,
    })
}

async fn fetch_settings_and_profiles(pool: &sqlx::SqlitePool) -> Result<Value, BffError> {
    let settings =
        sqlx::query_as::<_, SettingsRow>("SELECT * FROM hypervisor_settings WHERE id = 1")
            .fetch_one(pool)
            .await
            .map_err(|e| {
                BffError::Internal(format!("failed to fetch hypervisor settings: {}", e))
            })?;

    let profiles = sqlx::query_as::<_, ProfileRow>("SELECT * FROM hypervisor_profiles")
        .fetch_all(pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to fetch hypervisor profiles: {}", e)))?;

    Ok(json!({
        "settings": settings_to_json(settings),
        "profiles": profiles.into_iter().map(profile_to_json).collect::<Vec<Value>>(),
    }))
}

// ------------------------------------------------------------------
// Handlers
// ------------------------------------------------------------------

pub async fn get_settings(
    BearerToken(_claims): BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let result = fetch_settings_and_profiles(&state.pool).await?;
    Ok(Json(result))
}

pub async fn update_settings(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;

    let input = HypervisorSettingsPatchInput {
        cpu_nested: payload.get("cpu_nested").and_then(|v| v.as_bool()),
        cpu_amx: payload.get("cpu_amx").and_then(|v| v.as_bool()),
        cpu_kvm_hyperv: payload.get("cpu_kvm_hyperv").and_then(|v| v.as_bool()),
        memory_mergeable: payload.get("memory_mergeable").and_then(|v| v.as_bool()),
        memory_hugepages: payload.get("memory_hugepages").and_then(|v| v.as_bool()),
        memory_shared: payload.get("memory_shared").and_then(|v| v.as_bool()),
        memory_prefault: payload.get("memory_prefault").and_then(|v| v.as_bool()),
        iommu: payload.get("iommu").and_then(|v| v.as_bool()),
        rng_src: payload
            .get("rng_src")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        watchdog: payload.get("watchdog").and_then(|v| v.as_bool()),
        landlock_enable: payload.get("landlock_enable").and_then(|v| v.as_bool()),
        serial_mode: payload
            .get("serial_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        console_mode: payload
            .get("console_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        pvpanic: payload.get("pvpanic").and_then(|v| v.as_bool()),
        tpm_type: payload
            .get("tpm_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tpm_socket_path: payload
            .get("tpm_socket_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        profile_id: payload
            .get("profile_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    if let Err(msg) = validate_settings_patch(&input) {
        return Err(BffError::BadRequest(msg));
    }

    // Build dynamic UPDATE
    let mut sets: Vec<&'static str> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    macro_rules! push_bool {
        ($field:ident, $col:literal) => {
            if let Some(v) = input.$field {
                sets.push(concat!($col, " = ?"));
                params.push(json!(v));
            }
        };
    }
    macro_rules! push_string {
        ($field:ident, $col:literal) => {
            if let Some(ref v) = input.$field {
                sets.push(concat!($col, " = ?"));
                params.push(json!(v));
            }
        };
    }

    push_bool!(cpu_nested, "cpu_nested");
    push_bool!(cpu_amx, "cpu_amx");
    push_bool!(cpu_kvm_hyperv, "cpu_kvm_hyperv");
    push_bool!(memory_mergeable, "memory_mergeable");
    push_bool!(memory_hugepages, "memory_hugepages");
    push_bool!(memory_shared, "memory_shared");
    push_bool!(memory_prefault, "memory_prefault");
    push_bool!(iommu, "iommu");
    push_string!(rng_src, "rng_src");
    push_bool!(watchdog, "watchdog");
    push_bool!(landlock_enable, "landlock_enable");
    push_string!(serial_mode, "serial_mode");
    push_string!(console_mode, "console_mode");
    push_bool!(pvpanic, "pvpanic");
    push_string!(tpm_type, "tpm_type");
    push_string!(tpm_socket_path, "tpm_socket_path");
    push_string!(profile_id, "profile_id");

    if !sets.is_empty() {
        sets.push("updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')");
        let sql = format!(
            "UPDATE hypervisor_settings SET {} WHERE id = 1",
            sets.join(", ")
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        for p in params {
            query = query.bind(p);
        }
        query.execute(&state.pool).await.map_err(|e| {
            BffError::Internal(format!("failed to update hypervisor settings: {}", e))
        })?;
    }

    let result = fetch_settings_and_profiles(&state.pool).await?;
    Ok(Json(result))
}

async fn apply_profile_inner(
    claims: &crate::auth::Claims,
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(claims)?;

    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM hypervisor_profiles WHERE id = ?")
            .bind(profile_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| BffError::Internal(format!("failed to check profile: {}", e)))?;

    if exists.is_none() {
        return Err(BffError::NotFound(format!(
            "profile {} not found",
            profile_id
        )));
    }

    sqlx::query(
        r#"
        UPDATE hypervisor_settings SET
            cpu_nested = COALESCE((SELECT cpu_nested FROM hypervisor_profiles WHERE id = ?), cpu_nested),
            cpu_amx = COALESCE((SELECT cpu_amx FROM hypervisor_profiles WHERE id = ?), cpu_amx),
            cpu_kvm_hyperv = COALESCE((SELECT cpu_kvm_hyperv FROM hypervisor_profiles WHERE id = ?), cpu_kvm_hyperv),
            memory_mergeable = COALESCE((SELECT memory_mergeable FROM hypervisor_profiles WHERE id = ?), memory_mergeable),
            memory_hugepages = COALESCE((SELECT memory_hugepages FROM hypervisor_profiles WHERE id = ?), memory_hugepages),
            memory_shared = COALESCE((SELECT memory_shared FROM hypervisor_profiles WHERE id = ?), memory_shared),
            memory_prefault = COALESCE((SELECT memory_prefault FROM hypervisor_profiles WHERE id = ?), memory_prefault),
            iommu = COALESCE((SELECT iommu FROM hypervisor_profiles WHERE id = ?), iommu),
            rng_src = COALESCE((SELECT rng_src FROM hypervisor_profiles WHERE id = ?), rng_src),
            watchdog = COALESCE((SELECT watchdog FROM hypervisor_profiles WHERE id = ?), watchdog),
            landlock_enable = COALESCE((SELECT landlock_enable FROM hypervisor_profiles WHERE id = ?), landlock_enable),
            serial_mode = COALESCE((SELECT serial_mode FROM hypervisor_profiles WHERE id = ?), serial_mode),
            console_mode = COALESCE((SELECT console_mode FROM hypervisor_profiles WHERE id = ?), console_mode),
            pvpanic = COALESCE((SELECT pvpanic FROM hypervisor_profiles WHERE id = ?), pvpanic),
            tpm_type = COALESCE((SELECT tpm_type FROM hypervisor_profiles WHERE id = ?), tpm_type),
            tpm_socket_path = COALESCE((SELECT tpm_socket_path FROM hypervisor_profiles WHERE id = ?), tpm_socket_path),
            profile_id = ?,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
        WHERE id = 1
        "#,
    )
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .execute(pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to apply profile: {}", e)))?;

    let result = fetch_settings_and_profiles(pool).await?;
    Ok(Json(result))
}

pub async fn apply_profile(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let profile_id = payload
        .get("profile_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing profile_id".into()))?;
    apply_profile_inner(&claims, &state.pool, profile_id).await
}

pub async fn apply_profile_by_path(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    axum::extract::Path(profile_id): axum::extract::Path<String>,
) -> Result<Json<Value>, BffError> {
    apply_profile_inner(&claims, &state.pool, &profile_id).await
}

pub async fn list_profiles(
    BearerToken(_claims): BearerToken,
    State(state): State<AppState>,
) -> Result<Json<Value>, BffError> {
    let profiles = sqlx::query_as::<_, ProfileRow>("SELECT * FROM hypervisor_profiles")
        .fetch_all(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to fetch hypervisor profiles: {}", e)))?;

    Ok(Json(json!({
        "profiles": profiles.into_iter().map(profile_to_json).collect::<Vec<Value>>(),
    })))
}
