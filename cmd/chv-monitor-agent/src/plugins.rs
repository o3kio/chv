//! Opt-in local plugin sandbox (prompt 04, G4 part 1).
//!
//! Authority: `docs/specs/contracts/chv-monitor-agent-security-plugins-v1.md`
//! ("Plugin manifest v1", "Plugin output v1", "Execution limits",
//! "Required negative tests") and
//! `docs/specs/component/chv-monitor-agent-spec.md` ("Plugin model").
//! Shipped operator documentation lives in `docs/examples/plugins/README.md`
//! — the behavior implemented here must match it exactly.
//!
//! # Security model
//!
//! Plugins are a purely LOCAL, root-administrator-owned mechanism.
//! The manager can never push, enable, or configure a plugin: there
//! is no manager-to-agent plugin channel in v1, and nothing in this
//! module performs any network I/O (the manager-not-involved
//! invariant — no reqwest, no sockets; only local files and child
//! processes).
//!
//! Before EVERY run (not just at load) the engine re-verifies:
//!
//! * the manifest — a `*.json` regular file, no symlink, root-owned
//!   (`uid == 0`), parsing as manifest v1 with every field rule
//!   enforced (typed [`PluginManifestError`]);
//! * the executable — absolute path, regular file, no symlink,
//!   canonicalizing INSIDE the allowlist directory (path traversal
//!   and symlink escapes are rejected), root-owned, owner-executable,
//!   and its streamed SHA-256 matching the manifest's pinned digest.
//!   A modified executable fails the check as `unknown` — it is
//!   NEVER executed.
//!
//! Execution itself is a sandbox: no shell (direct exec of the
//! verified canonical path, no arguments), a minimal fixed
//! environment (`PATH`, `LANG` — never the agent's environment, so
//! no secrets or manager URLs leak), a fresh process group per run
//! (`process_group(0)`) so a timeout or output overflow SIGKILLs the
//! plugin AND all its descendants, streamed stdout capped at
//! `max_output_bytes` (never buffered unbounded; stderr goes to
//! `/dev/null` so plugin stderr never enters telemetry), and a
//! per-agent global concurrency cap of 2 enforced by an
//! engine-owned [`tokio::sync::Semaphore`].
//!
//! # Honest degradation
//!
//! Every failure — manifest invalid, verification refused, spawn
//! error, timeout, output overflow, non-zero exit, unparseable or
//! rule-violating output — degrades that plugin's check to a single
//! `unknown` [`CheckOutcome`] with a structured summary. The engine
//! never panics, never reports `ok` for a plugin it could not fully
//! verify and run, and never touches VM lifecycle.
//!
//! # Record identity and metrics
//!
//! Check ids are namespaced by contract (`plugin:<id>`, the same
//! convention as `service:<unit>` and `http:<label>`): the manifest
//! must declare `plugin:`-prefixed ids, the plugin must report one of
//! them verbatim, and the record carries it verbatim — the engine
//! never re-prefixes (a double namespace would fragment inventory).
//! A rejected manifest falls back to the synthesized
//! `plugin:<manifest-file-stem>`. A check id the manifest does not
//! allowlist is never echoed into a record — a malicious plugin
//! cannot mint arbitrary check inventory.
//! `service_key` is always `None`.
//!
//! Plugin-declared metrics are validated against the metric registry
//! (any unregistered id degrades the whole result) but are NOT
//! forwarded: the agent is the authoritative measurer and the only
//! writer of `check.*` samples (see `crate::checks`), so a plugin's
//! own timing claims are ignored — the engine measures wall-clock
//! duration itself and fills `CheckOutcome::duration_ms`.
//!
//! # Due-ness state
//!
//! [`PluginEngine::run_due`] takes `last_run_ms` from the caller
//! rather than holding it in the engine: the engine stays stateless
//! (constructible per tick, shareable), the caller decides where the
//! map lives, and an agent restart simply re-runs due plugins once —
//! bounded by the concurrency cap — instead of persisting opaque
//! plugin state.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

use chv_monitoring_core::model::CheckStatus;

use crate::checks::CheckOutcome;
use crate::wire::{CheckJson, SCHEMA_VERSION};

/// Contract "Execution limits": at most this many manifests are
/// processed per cycle (sorted by filename; the rest are skipped
/// with a warning).
pub const MAX_MANIFESTS: usize = 8;
/// Contract "Execution limits": at most 8 checks per plugin.
pub const MAX_CHECKS_PER_PLUGIN: usize = 8;
/// Manifest v1 `plugin_id` bound.
pub const MAX_PLUGIN_ID_BYTES: usize = 128;
/// Contract "Execution limits": summaries are bounded, never
/// truncated — an over-long or non-printable summary degrades to
/// `unknown`.
pub const MAX_SUMMARY_BYTES: usize = 256;
/// Contract "Execution limits": per-agent global plugin concurrency
/// cap of 2.
pub const PLUGIN_CONCURRENCY: usize = 2;
/// The only environment a plugin ever sees (contract: minimal fixed
/// environment; the agent's own environment — credentials, manager
/// URLs — is never inherited).
const PLUGIN_PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const PLUGIN_LANG_ENV: &str = "C.UTF-8";
/// Stdout is read incrementally with one byte of headroom past the
/// cap so "exceeded" is detectable without ever buffering more.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// Manifest v1 (security/plugins contract "Plugin manifest v1").
///
/// Deserialized strictly ([`serde(deny_unknown_fields)`]) — unknown
/// keys are a parse failure, never silently ignored. Field rules are
/// enforced by [`PluginManifest::validate_fields`] with typed errors.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Exactly `1`.
    pub schema_version: i32,
    /// Stable identity, ≤ 128 bytes of `[A-Za-z0-9._:/-]`.
    pub plugin_id: String,
    /// Human-facing plugin version (not used for any decision).
    pub plugin_version: String,
    /// Absolute path; must resolve inside the allowlist directory.
    pub executable: PathBuf,
    /// SHA-256 of the executable bytes: exactly 64 lowercase hex
    /// characters, re-verified before every execution.
    pub sha256: String,
    /// The `check_id`s this plugin may report (1..=8). Anything else
    /// in its output degrades the result to `unknown`.
    pub checks: Vec<String>,
    /// Execution interval, 1..=600 seconds.
    pub interval_seconds: u64,
    /// Wall timeout, 1..=30 seconds (contract default 5).
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Stdout cap, 1..=262144 bytes (contract default 32768).
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Must be exactly `unprivileged` in v1 — a plugin needing
    /// privilege must not exist.
    pub privilege_profile: String,
}

fn default_timeout_seconds() -> u64 {
    5
}

fn default_max_output_bytes() -> usize {
    32_768
}

/// Typed manifest failure: I/O, strict parse, or a field-rule
/// violation (mirrors `config::ConfigError`'s shape).
#[derive(Debug, thiserror::Error)]
pub enum PluginManifestError {
    #[error("failed to read plugin manifest: {0}")]
    Io(#[from] io::Error),
    #[error("failed to parse plugin manifest (strict v1, no unknown fields): {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid plugin manifest: {0}")]
    Invalid(String),
}

impl PluginManifest {
    /// Enforce every manifest v1 field rule (contract "Plugin
    /// manifest v1" / README field table). On success the invariant
    /// `checks` is non-empty holds — callers rely on `checks[0]`
    /// for the failure-record check id.
    pub fn validate_fields(&self) -> Result<(), PluginManifestError> {
        if self.schema_version != 1 {
            return Err(PluginManifestError::Invalid(
                "schema_version must be exactly 1".into(),
            ));
        }
        if self.plugin_id.is_empty()
            || self.plugin_id.len() > MAX_PLUGIN_ID_BYTES
            || !self
                .plugin_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'))
        {
            return Err(PluginManifestError::Invalid(format!(
                "plugin_id must be 1..={MAX_PLUGIN_ID_BYTES} bytes of [A-Za-z0-9._:/-]"
            )));
        }
        if !self.executable.is_absolute() {
            return Err(PluginManifestError::Invalid(
                "executable must be an absolute path".into(),
            ));
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(PluginManifestError::Invalid(
                "sha256 must be exactly 64 lowercase hex characters".into(),
            ));
        }
        if self.checks.is_empty() || self.checks.len() > MAX_CHECKS_PER_PLUGIN {
            return Err(PluginManifestError::Invalid(format!(
                "checks must list 1..={MAX_CHECKS_PER_PLUGIN} check ids"
            )));
        }
        for check in &self.checks {
            // These become record check ids — bound them so a
            // malicious manifest cannot mint unbounded inventory,
            // require the contract's `plugin:` namespace so record
            // identity is unambiguous (the engine uses them verbatim
            // and never re-prefixes), and pin them to the
            // manager-side check-id charset: an id outside
            // [A-Za-z0-9._:/@-] is a whole-batch rejection on the
            // manager (the G4 instance-unit lesson), so the agent
            // must refuse such a manifest at load instead of
            // poisoning every envelope it touches.
            if check.is_empty()
                || check.len() > MAX_PLUGIN_ID_BYTES
                || !check.bytes().all(in_check_id_charset)
            {
                return Err(PluginManifestError::Invalid(format!(
                    "check id {check:?} must be 1..={MAX_PLUGIN_ID_BYTES} bytes of \
                     [A-Za-z0-9._:/@-] (the manager's check-id charset)"
                )));
            }
            if !check.starts_with("plugin:") {
                return Err(PluginManifestError::Invalid(format!(
                    "check id {check:?} must be plugin:-prefixed (namespaced check ids, \
                     contract v1)"
                )));
            }
        }
        if !(1..=600).contains(&self.interval_seconds) {
            return Err(PluginManifestError::Invalid(
                "interval_seconds must be in 1..=600".into(),
            ));
        }
        if !(1..=30).contains(&self.timeout_seconds) {
            return Err(PluginManifestError::Invalid(
                "timeout_seconds must be in 1..=30".into(),
            ));
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > 262_144 {
            return Err(PluginManifestError::Invalid(
                "max_output_bytes must be in 1..=262144".into(),
            ));
        }
        if self.privilege_profile != "unprivileged" {
            return Err(PluginManifestError::Invalid(
                "privilege_profile must be \"unprivileged\" in v1".into(),
            ));
        }
        Ok(())
    }
}

/// Plugin output v1 (contract "Plugin output v1"), parsed strictly.
/// Metrics are validated against the registry but never forwarded —
/// the agent is the authoritative measurer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginOutput {
    schema_version: i32,
    check_id: String,
    status: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    metrics: Vec<PluginMetric>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginMetric {
    metric_id: String,
    #[allow(dead_code)]
    value: serde_json::Number,
    #[allow(dead_code)]
    unit: String,
}

/// A fully validated plugin output: the reported check id (already
/// allowlisted), the parsed status, and the bounded summary.
#[derive(Debug)]
struct ParsedOutput {
    check_id: String,
    status: CheckStatus,
    summary: Option<String>,
}

/// Why an output degraded to `unknown`. `check_id` carries the
/// reported id ONLY once it has been validated against the
/// manifest's allowlist — an unvalidated plugin-reported id is never
/// used for record identity.
#[derive(Debug)]
struct OutputParseError {
    reason: &'static str,
    check_id: Option<String>,
}

/// The opt-in plugin sandbox engine. Construct one per agent (or per
/// tick); the concurrency semaphore is owned here so the cap of
/// [`PLUGIN_CONCURRENCY`] holds agent-wide no matter how many
/// engines' callers overlap.
pub struct PluginEngine {
    directory: PathBuf,
    semaphore: Arc<Semaphore>,
}

impl PluginEngine {
    /// `directory` is the root-owned allowlist directory (already
    /// validated absolute by `config::PluginsConfig`).
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            semaphore: Arc::new(Semaphore::new(PLUGIN_CONCURRENCY)),
        }
    }

    /// Verify and run every due manifest, returning one
    /// [`CheckOutcome`] per attempted plugin (multiple only when a
    /// run succeeded — a failure always yields exactly one
    /// `unknown`).
    ///
    /// `now_ms` drives per-plugin interval due-ness: a plugin that
    /// is not due is skipped this cycle (no outcome — its last
    /// record stands). A backwards clock jump makes plugins due
    /// immediately rather than freezing them. `last_run_ms` is
    /// caller-owned so the engine stays stateless across restarts;
    /// every attempted plugin (successful or degraded) is stamped
    /// with `now_ms`.
    ///
    /// Never fails: discovery errors degrade to an empty list and
    /// per-plugin problems degrade to `unknown` outcomes with
    /// structured summaries. Due plugins run concurrently, bounded
    /// by the engine's semaphore.
    pub async fn run_due(
        &self,
        now_ms: i64,
        last_run_ms: &mut HashMap<String, i64>,
    ) -> Vec<CheckOutcome> {
        let dir = match std::fs::canonicalize(&self.directory) {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(
                    directory = %self.directory.display(),
                    error = %e,
                    "plugin allowlist directory unavailable — no plugin checks this cycle"
                );
                return Vec::new();
            }
        };
        let mut manifests: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                // Non-recursive: only direct *.json children.
                .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
                .collect(),
            Err(e) => {
                tracing::warn!(
                    directory = %dir.display(),
                    error = %e,
                    "plugin allowlist directory unreadable — no plugin checks this cycle"
                );
                return Vec::new();
            }
        };
        manifests.sort();
        if manifests.len() > MAX_MANIFESTS {
            tracing::warn!(
                found = manifests.len(),
                processed = MAX_MANIFESTS,
                "plugin allowlist directory exceeds the manifest cap — processing the first 8 by filename"
            );
            manifests.truncate(MAX_MANIFESTS);
        }

        let mut outcomes = Vec::new();
        let mut due = Vec::new();
        for path in manifests {
            match load_manifest(&path).await {
                Ok(manifest) => due.push(manifest),
                Err(e) => {
                    // The manifest was never accepted, so its
                    // self-asserted plugin_id carries no weight —
                    // the record id falls back to a sanitized file
                    // stem (never raw filename bytes: a stem outside
                    // the manager's check-id charset would poison
                    // the whole batch).
                    let stem = file_stem_string(&path);
                    tracing::warn!(manifest = %stem, error = %e, "plugin manifest rejected");
                    outcomes.push(unknown_outcome(
                        &synthesized_record_id(&stem),
                        format!("plugin manifest invalid: {e}"),
                        now_ms,
                        Instant::now(),
                    ));
                }
            }
        }

        let mut handles = Vec::new();
        for manifest in due {
            let due_now = match last_run_ms.get(&manifest.plugin_id).copied() {
                None => true,
                Some(last) => {
                    let delta = now_ms - last;
                    delta < 0 || delta >= (manifest.interval_seconds as i64) * 1000
                }
            };
            if !due_now {
                continue;
            }
            last_run_ms.insert(manifest.plugin_id.clone(), now_ms);
            let dir = dir.clone();
            let semaphore = Arc::clone(&self.semaphore);
            handles.push(tokio::spawn(run_one(dir, semaphore, manifest, now_ms)));
        }
        for handle in handles {
            match handle.await {
                Ok(mut run) => outcomes.append(&mut run),
                Err(e) => {
                    // A task panic is an engine bug; degrade, never
                    // propagate. run_one is written not to panic.
                    tracing::warn!(error = %e, "plugin run task failed");
                    outcomes.push(unknown_outcome(
                        "plugin:engine",
                        "plugin engine run task failed".to_string(),
                        now_ms,
                        Instant::now(),
                    ));
                }
            }
        }
        outcomes
    }
}

/// Load and fully validate one manifest file: regular file, no
/// symlink, root-owned, strict v1 parse, all field rules. This —
/// plus [`verify_executable`] — runs before EVERY execution, not
/// just at load.
async fn load_manifest(path: &Path) -> Result<PluginManifest, PluginManifestError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).map_err(PluginManifestError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(PluginManifestError::Invalid("manifest is a symlink".into()));
    }
    if !metadata.is_file() {
        return Err(PluginManifestError::Invalid(
            "manifest is not a regular file".into(),
        ));
    }
    if metadata.uid() != 0 {
        return Err(PluginManifestError::Invalid(
            "manifest is not root-owned".into(),
        ));
    }
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(PluginManifestError::Io)?;
    let manifest: PluginManifest =
        serde_json::from_str(&text).map_err(PluginManifestError::Parse)?;
    manifest.validate_fields()?;
    Ok(manifest)
}

/// Verify the manifest's executable before execution and return its
/// canonical path: regular file, no symlink, root-owned,
/// owner-executable, canonicalizing inside `dir`, streamed SHA-256
/// equal to the pinned digest. A modified executable fails here and
/// is never executed. (There is an unavoidable verify-then-exec
/// window on the same path; the contract's model is verify-before-
/// every-run, which this implements.)
async fn verify_executable(dir: &Path, manifest: &PluginManifest) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(&manifest.executable)
        .map_err(|e| format!("executable metadata unavailable: {e}"))?;
    if metadata.file_type().is_symlink() {
        return Err("executable is a symlink".into());
    }
    if !metadata.is_file() {
        return Err("executable is not a regular file".into());
    }
    if metadata.uid() != 0 {
        return Err("executable is not root-owned".into());
    }
    if metadata.mode() & 0o100 == 0 {
        return Err("executable is not executable by its owner".into());
    }
    let canonical = std::fs::canonicalize(&manifest.executable)
        .map_err(|e| format!("executable path unresolvable: {e}"))?;
    // Path traversal (`..`) and symlink escapes both canonicalize
    // outside the allowlist and are rejected here.
    if !is_inside_dir(dir, &canonical) {
        return Err("executable resolves outside the plugin allowlist directory".into());
    }
    let digest = sha256_file(&canonical)
        .await
        .map_err(|e| format!("executable unreadable: {e}"))?;
    if digest != manifest.sha256 {
        return Err("executable digest does not match the pinned sha256".into());
    }
    Ok(canonical)
}

/// Streamed SHA-256 (never loads the whole executable into memory).
async fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = file.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Component-wise containment: `candidate` must be strictly inside
/// `dir` (both canonical).
fn is_inside_dir(dir: &Path, candidate: &Path) -> bool {
    match candidate.strip_prefix(dir) {
        Ok(rest) => !rest.as_os_str().is_empty(),
        Err(_) => false,
    }
}

/// One plugin's full run: acquire a concurrency permit, verify,
/// execute, parse. Returns exactly one outcome.
async fn run_one(
    dir: PathBuf,
    semaphore: Arc<Semaphore>,
    manifest: PluginManifest,
    now_ms: i64,
) -> Vec<CheckOutcome> {
    let started = Instant::now();
    // validate_fields guarantees non-empty, plugin:-prefixed checks;
    // ids are used verbatim (never re-prefixed).
    let fallback_id = manifest.checks[0].clone();
    let _permit = match semaphore.acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            // Only possible if the semaphore were closed; it never
            // is. Degrade rather than run unbounded.
            return vec![unknown_outcome(
                &fallback_id,
                "plugin engine semaphore closed".to_string(),
                now_ms,
                started,
            )];
        }
    };
    let executable = match verify_executable(&dir, &manifest).await {
        Ok(path) => path,
        Err(reason) => {
            tracing::warn!(
                plugin = %manifest.plugin_id,
                reason = %reason,
                "plugin refused — verification failed, not executed"
            );
            return vec![unknown_outcome(
                &fallback_id,
                format!("plugin verification failed: {reason}"),
                now_ms,
                started,
            )];
        }
    };
    match execute(&manifest, &executable).await {
        ExecOutcome::Failed(reason) => vec![unknown_outcome(&fallback_id, reason, now_ms, started)],
        ExecOutcome::Stdout(bytes) => match parse_output(&bytes, &manifest) {
            Ok(parsed) => vec![CheckOutcome {
                check: CheckJson {
                    schema_version: SCHEMA_VERSION,
                    // Verbatim: the reported id was validated against
                    // the manifest's plugin:-prefixed allowlist.
                    check_id: parsed.check_id,
                    service_key: None,
                    status: parsed.status.as_str().to_string(),
                    summary: parsed.summary,
                    observed_at_ms: now_ms,
                },
                duration_ms: elapsed_ms(started),
            }],
            Err(e) => {
                // Use the reported id only once it passed the
                // allowlist; otherwise the manifest's declared id.
                let check_id = e.check_id.unwrap_or_else(|| fallback_id.clone());
                tracing::warn!(
                    plugin = %manifest.plugin_id,
                    reason = e.reason,
                    "plugin output degraded to unknown"
                );
                vec![unknown_outcome(
                    &check_id,
                    e.reason.to_string(),
                    now_ms,
                    started,
                )]
            }
        },
    }
}

enum ExecOutcome {
    /// Process exited 0 with at most `max_output_bytes` of stdout.
    Stdout(Vec<u8>),
    /// Degrade-to-unknown reason (timeout, overflow, exit status…).
    Failed(String),
}

enum StreamFail {
    /// Output would exceed the cap (+1 byte of headroom).
    Exceeded,
    Read(io::Error),
    Wait(io::Error),
}

/// The sandboxed execution: direct exec (no shell, no arguments),
/// minimal fixed environment, own process group, streamed and capped
/// stdout, stderr discarded, wall timeout with a group-wide SIGKILL.
async fn execute(manifest: &PluginManifest, executable: &Path) -> ExecOutcome {
    let mut command = Command::new(executable);
    command
        // Minimal fixed environment — never the agent's.
        .env_clear()
        .env("PATH", PLUGIN_PATH_ENV)
        .env("LANG", PLUGIN_LANG_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Plugin stderr never enters telemetry.
        .stderr(Stdio::null())
        // Backstop for the abnormal-drop path; the normal timeout
        // and overflow paths kill the whole group explicitly.
        .kill_on_drop(true)
        // Fresh process group: the plugin and all descendants form
        // one killable group.
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return ExecOutcome::Failed(format!("plugin failed to start: {e}")),
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let cap = manifest.max_output_bytes;
    let result = tokio::time::timeout(Duration::from_secs(manifest.timeout_seconds), async {
        let mut out = Vec::new();
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    // Stop buffering at cap+1: the excess is
                    // detectable without ever holding more.
                    if out.len() + n > cap + 1 {
                        return Err(StreamFail::Exceeded);
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
                Err(e) => return Err(StreamFail::Read(e)),
            }
        }
        child
            .wait()
            .await
            .map(|status| (out, status))
            .map_err(StreamFail::Wait)
    })
    .await;

    match result {
        // Deadline elapsed: SIGKILL the whole group (descendants
        // die too) and reap.
        Err(_elapsed) => {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            ExecOutcome::Failed("plugin timed out".into())
        }
        Ok(Err(StreamFail::Exceeded)) => {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            ExecOutcome::Failed("plugin output exceeded limit".into())
        }
        Ok(Err(StreamFail::Read(e))) => {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            ExecOutcome::Failed(format!("plugin stdout read failed: {e}"))
        }
        Ok(Err(StreamFail::Wait(e))) => {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            ExecOutcome::Failed(format!("plugin process wait failed: {e}"))
        }
        Ok(Ok((out, status))) => {
            if out.len() > cap {
                // Exactly cap+1 bytes were buffered before EOF: kill
                // any lingering group members (the leader already
                // exited) and degrade.
                kill_process_group(&mut child);
                ExecOutcome::Failed("plugin output exceeded limit".into())
            } else if !status.success() {
                match status.code() {
                    Some(code) => ExecOutcome::Failed(format!("plugin exited with status {code}")),
                    None => ExecOutcome::Failed("plugin terminated by a signal".into()),
                }
            } else {
                ExecOutcome::Stdout(out)
            }
        }
    }
}

/// SIGKILL the child's whole process group (the descendant-kill
/// guarantee). The child is reaped by the caller afterwards.
fn kill_process_group(child: &mut Child) {
    let Some(pid) = child.id() else { return };
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if rc != 0 {
        tracing::warn!(pid, "failed to SIGKILL the plugin process group");
    }
}

/// Parse plugin output v1 against the manifest's allowlist and the
/// contract's vocabulary rules. Every violation degrades the whole
/// result (never a partial `ok`).
fn parse_output(raw: &[u8], manifest: &PluginManifest) -> Result<ParsedOutput, OutputParseError> {
    let output: PluginOutput = serde_json::from_slice(raw).map_err(|_| OutputParseError {
        reason: "plugin output unparseable",
        check_id: None,
    })?;
    if output.schema_version != 1 {
        return Err(OutputParseError {
            reason: "plugin output schema version is not 1",
            check_id: None,
        });
    }
    if !manifest.checks.iter().any(|c| c == &output.check_id) {
        return Err(OutputParseError {
            reason: "plugin reported an unexpected check id",
            check_id: None,
        });
    }
    let status = CheckStatus::parse(&output.status).ok_or_else(|| OutputParseError {
        reason: "plugin reported an invalid status",
        check_id: Some(output.check_id.clone()),
    })?;
    // ONLY registry metrics are allowed; the values are never
    // forwarded (the agent is the authoritative measurer) — this is
    // an inventory-integrity gate, not a relay.
    for metric in &output.metrics {
        if chv_monitoring_core::registry::lookup(&metric.metric_id).is_none() {
            return Err(OutputParseError {
                reason: "plugin reported an unregistered metric",
                check_id: Some(output.check_id.clone()),
            });
        }
    }
    // Summaries are bounded and printable — rejected whole, never
    // truncated (a secret-bearing or oversized summary must not
    // reach telemetry in any form).
    if let Some(summary) = &output.summary {
        if summary.len() > MAX_SUMMARY_BYTES || summary.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(OutputParseError {
                reason: "plugin summary rejected",
                check_id: Some(output.check_id.clone()),
            });
        }
    }
    Ok(ParsedOutput {
        check_id: output.check_id,
        status,
        summary: output.summary,
    })
}

/// One `unknown` record with a structured summary. `started` drives
/// the engine-measured duration (a plugin's own timing claims are
/// never trusted).
fn unknown_outcome(check_id: &str, summary: String, now_ms: i64, started: Instant) -> CheckOutcome {
    CheckOutcome {
        check: CheckJson {
            schema_version: SCHEMA_VERSION,
            check_id: check_id.to_string(),
            service_key: None,
            status: CheckStatus::Unknown.as_str().to_string(),
            summary: Some(summary),
            observed_at_ms: now_ms,
        },
        duration_ms: elapsed_ms(started),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// The manager-side check-id charset (ingestion contract v1):
/// [A-Za-z0-9._:/@-]. A record id outside it is a whole-batch
/// rejection — the G4 instance-unit lesson — so every agent-side
/// record-id source must stay inside it.
fn in_check_id_charset(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-' | b'@')
}

/// A record id synthesized for a manifest rejected before its
/// declared ids could be trusted. The stem is arbitrary
/// operator-side filename bytes and must not poison the batch:
/// charset-violating bytes map deterministically to `_`, and the
/// stem truncates to keep `plugin:` + stem inside the 128-byte
/// check-id cap. A stem that sanitizes to nothing falls back to
/// `unknown`.
fn synthesized_record_id(stem: &str) -> String {
    const PREFIX: &str = "plugin:";
    const MAX_STEM: usize = MAX_PLUGIN_ID_BYTES - PREFIX.len();
    let sanitized: String = stem
        .bytes()
        .take(MAX_STEM)
        .map(|b| {
            if in_check_id_charset(b) {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{PREFIX}{}",
        if sanitized.is_empty() {
            "unknown".to_string()
        } else {
            sanitized
        }
    )
}

fn file_stem_string(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::{chown, PermissionsExt};

    /// Ownership caveat: the acceptance path (root-owned fixtures
    /// passing verification) can only be exercised when the test
    /// process runs as root. Rejection paths run under any euid —
    /// when tests run as non-root, the files they create are simply
    /// not root-owned, which is itself the rejection path. This euid
    /// gate replaces any bypass flag in production code.
    fn running_as_root() -> bool {
        let euid = unsafe { libc::geteuid() };
        euid == 0
    }

    fn skip_acceptance(name: &str) {
        eprintln!(
            "skipping {name}: the root-owned acceptance path needs euid 0 \
             (rejection is covered by the non-root-owned default)"
        );
    }

    fn sha256_sync(path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(path).unwrap());
        hex::encode(hasher.finalize())
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn ok_output_json(check_id: &str) -> String {
        format!(
            r#"{{"schema_version":1,"check_id":"{check_id}","status":"ok","summary":"fine","metrics":[{{"metric_id":"check.duration_seconds","value":0.05,"unit":"seconds"}}]}}"#
        )
    }

    fn manifest_value(
        plugin_id: &str,
        executable: &Path,
        sha256: &str,
        checks: &[&str],
        timeout_seconds: u64,
        max_output_bytes: usize,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "plugin_id": plugin_id,
            "plugin_version": "1.0.0",
            "executable": executable.to_string_lossy(),
            "sha256": sha256,
            "checks": checks,
            "interval_seconds": 60,
            "timeout_seconds": timeout_seconds,
            "max_output_bytes": max_output_bytes,
            "privilege_profile": "unprivileged",
        })
    }

    /// Install a complete plugin (script + matching manifest) and
    /// return the manifest path. The script lives at `<stem>.sh`.
    fn install_plugin(
        dir: &Path,
        stem: &str,
        script_body: &str,
        checks: &[&str],
        timeout_seconds: u64,
        max_output_bytes: usize,
    ) -> PathBuf {
        let script = dir.join(format!("{stem}.sh"));
        write_script(&script, script_body);
        let manifest = dir.join(format!("{stem}.json"));
        let value = manifest_value(
            &format!("test.{stem}"),
            &script,
            &sha256_sync(&script),
            checks,
            timeout_seconds,
            max_output_bytes,
        );
        std::fs::write(&manifest, serde_json::to_string(&value).unwrap()).unwrap();
        manifest
    }

    async fn run_once(engine: &PluginEngine, now_ms: i64) -> Vec<CheckOutcome> {
        engine.run_due(now_ms, &mut HashMap::new()).await
    }

    fn assert_single_unknown(outcomes: &[CheckOutcome]) -> &CheckOutcome {
        assert_eq!(outcomes.len(), 1, "one degraded outcome, got {outcomes:?}");
        let outcome = &outcomes[0];
        assert_eq!(
            outcome.status(),
            CheckStatus::Unknown,
            "every failure must degrade to unknown, got {outcomes:?}"
        );
        assert!(outcome.check.summary.is_some());
        outcome
    }

    // ------------------------------------------------------------------
    // Positive path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn ok_plugin_produces_ok_outcome_with_engine_measured_duration() {
        if !running_as_root() {
            skip_acceptance("ok_plugin_produces_ok_outcome_with_engine_measured_duration");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "#!/bin/bash\nsleep 0.5\nprintf '%s' '{}'\n",
            ok_output_json("plugin:test.ok")
        );
        install_plugin(dir.path(), "ok", &body, &["plugin:test.ok"], 5, 32_768);
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 1_000_000).await;
        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0];
        assert_eq!(outcome.check.check_id, "plugin:test.ok");
        assert_eq!(outcome.status(), CheckStatus::Ok);
        assert_eq!(outcome.check.summary.as_deref(), Some("fine"));
        assert!(outcome.check.service_key.is_none());
        assert_eq!(outcome.check.schema_version, SCHEMA_VERSION);
        // Engine-measured duration: the plugin slept 0.5 s and its
        // own 0.05 s claim is ignored.
        assert!(
            outcome.duration_ms >= 400 && outcome.duration_ms < 5_000,
            "engine-measured duration must reflect the real run, got {}",
            outcome.duration_ms
        );
    }

    // ------------------------------------------------------------------
    // Verification failures — never executed
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn modified_executable_is_never_executed() {
        let dir = tempfile::tempdir().unwrap();
        let side_effect = dir.path().join("side-effect");
        let body = format!(
            "#!/bin/bash\necho ran >> {}\nprintf '%s' '{}'\n",
            side_effect.display(),
            ok_output_json("plugin:test.ok")
        );
        let script = dir.path().join("modified.sh");
        write_script(&script, &body);
        let manifest = dir.path().join("modified.json");
        std::fs::write(
            &manifest,
            serde_json::to_string(&manifest_value(
                "test.modified",
                &script,
                &sha256_sync(&script),
                &["plugin:test.ok"],
                5,
                32_768,
            ))
            .unwrap(),
        )
        .unwrap();
        // Tamper AFTER the manifest pinned the digest.
        std::fs::write(&script, format!("{body}# tampered\n")).unwrap();

        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert!(
            !side_effect.exists(),
            "a modified executable must never run"
        );
        if running_as_root() {
            assert!(
                outcome.check.summary.as_deref().unwrap().contains("digest"),
                "as root the refusal must be the digest mismatch, got {:?}",
                outcome.check.summary
            );
        }
    }

    #[tokio::test]
    async fn symlinked_executable_is_refused() {
        let outer = tempfile::tempdir().unwrap(); // outside the allowlist
        let dir = tempfile::tempdir().unwrap(); // the allowlist
        let side_effect = outer.path().join("side-effect");
        let outside = outer.path().join("outside.sh");
        write_script(
            &outside,
            &format!(
                "#!/bin/bash\necho ran >> {}\nprintf '%s' '{}'\n",
                side_effect.display(),
                ok_output_json("plugin:test.ok")
            ),
        );
        let link = dir.path().join("evil.sh");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let manifest = dir.path().join("evil.json");
        std::fs::write(
            &manifest,
            serde_json::to_string(&manifest_value(
                "test.evil",
                &link,
                &sha256_sync(&outside),
                &["plugin:test.ok"],
                5,
                32_768,
            ))
            .unwrap(),
        )
        .unwrap();

        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert!(
            !side_effect.exists(),
            "a symlinked executable must never run"
        );
        if running_as_root() {
            assert!(
                outcome
                    .check
                    .summary
                    .as_deref()
                    .unwrap()
                    .contains("symlink"),
                "as root the refusal must be the symlink check, got {:?}",
                outcome.check.summary
            );
        }
    }

    #[tokio::test]
    async fn non_root_owned_manifest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let side_effect = dir.path().join("side-effect");
        let body = format!(
            "#!/bin/bash\necho ran >> {}\nprintf '%s' '{}'\n",
            side_effect.display(),
            ok_output_json("plugin:test.ok")
        );
        let manifest = install_plugin(dir.path(), "owned", &body, &["plugin:test.ok"], 5, 32_768);
        if running_as_root() {
            // As root, demote the manifest to a non-root owner. As
            // non-root the files are already not root-owned.
            chown(&manifest, Some(65534), None).unwrap();
        }

        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert!(
            !side_effect.exists(),
            "a non-root-owned manifest must never lead to execution"
        );
        if running_as_root() {
            assert!(
                outcome
                    .check
                    .summary
                    .as_deref()
                    .unwrap()
                    .contains("not root-owned"),
                "as root the refusal must be ownership, got {:?}",
                outcome.check.summary
            );
        }
    }

    #[tokio::test]
    async fn non_root_owned_executable_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let side_effect = dir.path().join("side-effect");
        let body = format!(
            "#!/bin/bash\necho ran >> {}\nprintf '%s' '{}'\n",
            side_effect.display(),
            ok_output_json("plugin:test.ok")
        );
        install_plugin(
            dir.path(),
            "exeowned",
            &body,
            &["plugin:test.ok"],
            5,
            32_768,
        );
        let script = dir.path().join("exeowned.sh");
        if running_as_root() {
            chown(&script, Some(65534), None).unwrap();
        }

        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert!(
            !side_effect.exists(),
            "a non-root-owned executable must never run"
        );
        if running_as_root() {
            assert!(
                outcome
                    .check
                    .summary
                    .as_deref()
                    .unwrap()
                    .contains("not root-owned"),
                "as root the refusal must be ownership, got {:?}",
                outcome.check.summary
            );
        }
    }

    // ------------------------------------------------------------------
    // Execution limits
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn hanging_plugin_times_out_and_kills_its_process_group() {
        if !running_as_root() {
            skip_acceptance("hanging_plugin_times_out_and_kills_its_process_group");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let late = dir.path().join("late");
        // The plugin spawns a descendant that would touch `late`
        // after 2 s, then hangs for 30 s. timeout_seconds = 1.
        let body = format!(
            "#!/bin/bash\n(sleep 2; touch {}) &\nsleep 30\n",
            late.display()
        );
        install_plugin(dir.path(), "hang", &body, &["plugin:test.ok"], 1, 32_768);
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let started = Instant::now();
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        let elapsed = started.elapsed();
        assert!(
            outcome
                .check
                .summary
                .as_deref()
                .unwrap()
                .contains("timed out"),
            "expected the timeout summary, got {:?}",
            outcome.check.summary
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the 1 s timeout must fire promptly, took {elapsed:?}"
        );
        // Descendant-kill guarantee: if the group SIGKILL failed,
        // the orphaned `sleep 2; touch` pair would fire here.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            !late.exists(),
            "descendants of a timed-out plugin must die with the process group"
        );
    }

    #[tokio::test]
    async fn oversized_output_is_capped_and_killed() {
        if !running_as_root() {
            skip_acceptance("oversized_output_is_capped_and_killed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // An unbounded producer: only the output cap stops it (the
        // timeout is 30 s, so a pass here proves the kill).
        install_plugin(
            dir.path(),
            "loud",
            "#!/bin/bash\nyes abcdefghijklmnopqrstuvwxyz\n",
            &["plugin:test.ok"],
            30,
            1_024,
        );
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let started = Instant::now();
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert!(
            outcome
                .check
                .summary
                .as_deref()
                .unwrap()
                .contains("exceeded"),
            "expected the output-limit summary, got {:?}",
            outcome.check.summary
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "an over-cap producer must be killed at the cap, not at the 30 s timeout"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_degrades_to_unknown() {
        if !running_as_root() {
            skip_acceptance("nonzero_exit_degrades_to_unknown");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        install_plugin(
            dir.path(),
            "fail",
            "#!/bin/bash\nexit 3\n",
            &["plugin:test.ok"],
            5,
            32_768,
        );
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert_eq!(
            outcome.check.summary.as_deref(),
            Some("plugin exited with status 3")
        );
    }

    #[tokio::test]
    async fn unparseable_plugin_output_degrades_end_to_end() {
        if !running_as_root() {
            skip_acceptance("unparseable_plugin_output_degrades_end_to_end");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        install_plugin(
            dir.path(),
            "garbage",
            "#!/bin/bash\nprintf '%s' 'not json'\n",
            &["plugin:test.ok"],
            5,
            32_768,
        );
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        let outcome = assert_single_unknown(&outcomes);
        assert_eq!(
            outcome.check.summary.as_deref(),
            Some("plugin output unparseable")
        );
        // The record id falls back to the manifest's declared check
        // — the unparseable output mints no identity.
        assert_eq!(outcome.check.check_id, "plugin:test.ok");
    }

    // ------------------------------------------------------------------
    // Concurrency
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn engine_semaphore_permits_only_two_concurrent_plugins() {
        let engine = PluginEngine::new(PathBuf::from("/unused"));
        let semaphore = Arc::clone(&engine.semaphore);
        let first = semaphore.clone().acquire_owned().await.unwrap();
        let _second = semaphore.clone().acquire_owned().await.unwrap();
        assert!(
            semaphore.try_acquire().is_err(),
            "a third permit must not be available"
        );
        drop(first);
        assert!(semaphore.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn two_slow_plugins_run_concurrently_under_the_cap() {
        if !running_as_root() {
            skip_acceptance("two_slow_plugins_run_concurrently_under_the_cap");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for stem in ["slowa", "slowb"] {
            let check = format!("plugin:test.{stem}");
            let body = format!(
                "#!/bin/bash\nsleep 2\nprintf '%s' '{}'\n",
                ok_output_json(&check)
            );
            install_plugin(dir.path(), stem, &body, &[&check], 30, 32_768);
        }
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let started = Instant::now();
        let outcomes = run_once(&engine, 0).await;
        let elapsed = started.elapsed();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.status() == CheckStatus::Ok));
        assert!(elapsed >= Duration::from_millis(1_900), "both plugins ran");
        assert!(
            elapsed < Duration::from_millis(3_500),
            "two 2 s plugins must overlap under the cap of 2, took {elapsed:?}"
        );
    }

    // ------------------------------------------------------------------
    // Discovery, due-ness, degradation of the engine itself
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn missing_directory_degrades_to_no_outcomes() {
        let engine = PluginEngine::new(PathBuf::from("/nonexistent-chv-plugins-dir"));
        assert!(run_once(&engine, 0).await.is_empty());
    }

    #[tokio::test]
    async fn more_than_eight_manifests_process_only_the_first_eight() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=9 {
            // Field-valid manifests pointing at a missing executable:
            // every processed file yields exactly one degraded
            // outcome regardless of euid.
            let value = manifest_value(
                &format!("test.p{i}"),
                &dir.path().join(format!("missing{i}.sh")),
                &"a".repeat(64),
                &[&format!("c{i}")],
                5,
                32_768,
            );
            std::fs::write(
                dir.path().join(format!("p{i}.json")),
                serde_json::to_string(&value).unwrap(),
            )
            .unwrap();
        }
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let outcomes = run_once(&engine, 0).await;
        assert_eq!(outcomes.len(), 8, "only the first 8 manifests processed");
        assert!(
            !outcomes.iter().any(|o| o.check.check_id.contains("c9")),
            "the 9th manifest must be skipped: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn interval_gating_skips_plugins_not_yet_due() {
        // The scenario needs the manifest to LOAD (root-owned file):
        // the interval gate stamps last_run_ms only for accepted
        // manifests — a rejected one (the non-root default) reports
        // unknown every cycle by design.
        if !running_as_root() {
            skip_acceptance("interval_gating_skips_plugins_not_yet_due");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let value = manifest_value(
            "test.missing",
            &dir.path().join("missing.sh"),
            &"a".repeat(64),
            &["plugin:test.ok"],
            5,
            32_768,
        );
        std::fs::write(
            dir.path().join("missing.json"),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
        let engine = PluginEngine::new(dir.path().to_path_buf());
        let mut last_run_ms = HashMap::new();

        // First cycle: due (never run) — the attempt degrades to
        // unknown (executable missing) but still counts as a run.
        let first = engine.run_due(0, &mut last_run_ms).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status(), CheckStatus::Unknown);
        assert_eq!(last_run_ms.len(), 1);

        // 30 s later with a 60 s interval: not due, no outcome.
        assert!(engine.run_due(30_000, &mut last_run_ms).await.is_empty());

        // At the interval boundary: due again.
        assert_eq!(engine.run_due(60_000, &mut last_run_ms).await.len(), 1);
    }

    // ------------------------------------------------------------------
    // Output v1 parsing (pure — runs under any euid)
    // ------------------------------------------------------------------

    fn fixture_manifest() -> PluginManifest {
        PluginManifest {
            schema_version: 1,
            plugin_id: "test.plugin".into(),
            plugin_version: "1.0.0".into(),
            executable: "/etc/chv-monitor/plugins.d/x.sh".into(),
            sha256: "a".repeat(64),
            checks: vec!["plugin:test.ok".into()],
            interval_seconds: 60,
            timeout_seconds: 5,
            max_output_bytes: 32_768,
            privilege_profile: "unprivileged".into(),
        }
    }

    #[test]
    fn parse_output_accepts_contract_shape() {
        let manifest = fixture_manifest();
        let parsed = parse_output(ok_output_json("plugin:test.ok").as_bytes(), &manifest).unwrap();
        assert_eq!(parsed.check_id, "plugin:test.ok");
        assert_eq!(parsed.status, CheckStatus::Ok);
        assert_eq!(parsed.summary.as_deref(), Some("fine"));
    }

    #[test]
    fn parse_output_rejections() {
        let manifest = fixture_manifest();
        let long_summary = "s".repeat(300);
        let cases: Vec<(String, &str)> = vec![
            ("not json".to_string(), "unparseable"),
            (
                r#"{"schema_version":2,"check_id":"plugin:test.ok","status":"ok","summary":"s","metrics":[]}"#.into(),
                "schema version",
            ),
            (
                r#"{"schema_version":1,"check_id":"other.check","status":"ok","summary":"s","metrics":[]}"#.into(),
                "unexpected check id",
            ),
            (
                r#"{"schema_version":1,"check_id":"plugin:test.ok","status":"crispy","summary":"s","metrics":[]}"#.into(),
                "invalid status",
            ),
            (
                r#"{"schema_version":1,"check_id":"plugin:test.ok","status":"ok","summary":"s","metrics":[{"metric_id":"not.a.metric","value":1,"unit":"x"}]}"#.into(),
                "unregistered metric",
            ),
            (
                r#"{"schema_version":1,"check_id":"plugin:test.ok","status":"ok","summary":"bad\u0001char","metrics":[]}"#.into(),
                "summary rejected",
            ),
            (
                format!(r#"{{"schema_version":1,"check_id":"plugin:test.ok","status":"ok","summary":"{long_summary}","metrics":[]}}"#),
                "summary rejected",
            ),
            (
                r#"{"schema_version":1,"check_id":"plugin:test.ok","status":"ok","summary":"s","metrics":[],"evil":true}"#.into(),
                "unparseable",
            ),
            (
                r#"{"schema_version":1,"status":"ok","summary":"s","metrics":[]}"#.into(),
                "unparseable",
            ),
        ];
        for (raw, needle) in cases {
            let err = parse_output(raw.as_bytes(), &manifest).unwrap_err();
            assert!(
                err.reason.contains(needle),
                "{raw}: expected {needle:?}, got {:?}",
                err.reason
            );
        }
    }

    // ------------------------------------------------------------------
    // Manifest field validation (pure — runs under any euid)
    // ------------------------------------------------------------------

    fn validated(mutation: impl FnOnce(&mut serde_json::Value)) -> Result<(), PluginManifestError> {
        let mut value = serde_json::json!({
            "schema_version": 1,
            "plugin_id": "test.plugin",
            "plugin_version": "1.0.0",
            "executable": "/etc/chv-monitor/plugins.d/x.sh",
            "sha256": "a".repeat(64),
            "checks": ["plugin:test.ok"],
            "interval_seconds": 60,
            "timeout_seconds": 5,
            "max_output_bytes": 32_768,
            "privilege_profile": "unprivileged",
        });
        mutation(&mut value);
        let manifest: PluginManifest = serde_json::from_value(value).unwrap();
        manifest.validate_fields()
    }

    fn assert_invalid(mutation: impl FnOnce(&mut serde_json::Value), needle: &str) {
        let err = validated(mutation).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(needle),
            "expected {needle:?} in {message:?}"
        );
    }

    #[test]
    fn manifest_field_rules_match_the_contract() {
        assert_invalid(
            |v| v["schema_version"] = serde_json::json!(2),
            "schema_version",
        );
        assert_invalid(
            |v| v["sha256"] = serde_json::json!("z".repeat(64)),
            "sha256",
        );
        assert_invalid(
            |v| v["sha256"] = serde_json::json!("a".repeat(63)),
            "sha256",
        );
        assert_invalid(
            |v| v["sha256"] = serde_json::json!("A".repeat(64)),
            "sha256",
        );
        assert_invalid(
            |v| v["timeout_seconds"] = serde_json::json!(60),
            "timeout_seconds",
        );
        assert_invalid(
            |v| v["timeout_seconds"] = serde_json::json!(0),
            "timeout_seconds",
        );
        assert_invalid(
            |v| v["interval_seconds"] = serde_json::json!(0),
            "interval_seconds",
        );
        assert_invalid(
            |v| v["interval_seconds"] = serde_json::json!(601),
            "interval_seconds",
        );
        assert_invalid(
            |v| v["max_output_bytes"] = serde_json::json!(262_145),
            "max_output_bytes",
        );
        assert_invalid(
            |v| v["max_output_bytes"] = serde_json::json!(0),
            "max_output_bytes",
        );
        assert_invalid(
            |v| v["privilege_profile"] = serde_json::json!("root"),
            "privilege_profile",
        );
        assert_invalid(
            |v| {
                v["checks"] = serde_json::json!((0..9).map(|i| format!("c{i}")).collect::<Vec<_>>())
            },
            "checks",
        );
        assert_invalid(|v| v["checks"] = serde_json::json!([]), "checks");
        assert_invalid(
            |v| v["plugin_id"] = serde_json::json!("bad id!"),
            "plugin_id",
        );
        assert_invalid(
            |v| v["plugin_id"] = serde_json::json!("x".repeat(129)),
            "plugin_id",
        );
        assert_invalid(
            |v| v["executable"] = serde_json::json!("relative.sh"),
            "executable",
        );
        // Check ids must carry the contract's plugin: namespace —
        // the engine uses them verbatim for record identity.
        assert_invalid(
            |v| v["checks"] = serde_json::json!(["unprefixed.check"]),
            "plugin:",
        );
        // Check ids are record ids: the manager whole-batch-rejects
        // anything outside its check-id charset, so the agent must
        // refuse such a manifest at load instead of poisoning every
        // envelope it touches (the G4 instance-unit lesson).
        assert_invalid(
            |v| v["checks"] = serde_json::json!(["plugin:bad id"]),
            "check-id charset",
        );
        assert_invalid(
            |v| v["checks"] = serde_json::json!(["plugin:bad#id"]),
            "check-id charset",
        );
        // The instance-unit token stays valid — the manager charset
        // carries @.
        validated(|v| v["checks"] = serde_json::json!(["plugin:ok@id"])).unwrap();

        // The exact contract ceilings are accepted.
        validated(|v| {
            v["timeout_seconds"] = serde_json::json!(30);
            v["interval_seconds"] = serde_json::json!(600);
            v["max_output_bytes"] = serde_json::json!(262_144);
        })
        .unwrap();
    }

    #[test]
    fn manifest_defaults_and_strict_shape() {
        let text = format!(
            r#"{{"schema_version":1,"plugin_id":"test.plugin","plugin_version":"1.0.0","executable":"/etc/chv-monitor/plugins.d/x.sh","sha256":"{}","checks":["plugin:test.ok"],"interval_seconds":60,"privilege_profile":"unprivileged"}}"#,
            "a".repeat(64)
        );
        let manifest: PluginManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(manifest.timeout_seconds, 5);
        assert_eq!(manifest.max_output_bytes, 32_768);
        manifest.validate_fields().unwrap();

        // deny_unknown_fields: an extra key is a parse failure.
        let extra = text.replace("}", r#", "evil": true}"#);
        assert!(serde_json::from_str::<PluginManifest>(&extra).is_err());
    }

    #[test]
    fn synthesized_record_ids_stay_in_the_manager_charset() {
        // A rejected manifest's record id comes from its FILE NAME —
        // arbitrary operator-side bytes. Raw stems would poison the
        // whole batch on the manager (check-id charset,
        // whole-batch rejection); the synthesized id must sanitize.
        assert_eq!(synthesized_record_id("ok-plugin"), "plugin:ok-plugin");
        assert_eq!(synthesized_record_id("bad id"), "plugin:bad_id");
        assert_eq!(synthesized_record_id("bad#id"), "plugin:bad_id");
        // Multibyte UTF-8 maps per byte, deterministically.
        assert_eq!(synthesized_record_id("café"), "plugin:caf__");
        assert_eq!(synthesized_record_id("!!!"), "plugin:___");
        assert_eq!(synthesized_record_id(""), "plugin:unknown");
        // Length: `plugin:` + truncated stem never exceeds the
        // 128-byte check-id cap.
        let id = synthesized_record_id(&"a".repeat(300));
        assert_eq!(id.len(), MAX_PLUGIN_ID_BYTES);
        assert!(id.bytes().all(in_check_id_charset));
    }
}
