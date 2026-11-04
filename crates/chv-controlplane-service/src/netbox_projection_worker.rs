//! NetBox projection worker — the composition root of the projection
//! (PR 4 of the #239 plan).
//!
//! Mirrors [`crate::backup_worker::BackupWorker`]'s loop shape: a
//! clone-able struct, a `run(shutdown_rx)` tokio loop on a 30s tick,
//! `warn!`-and-continue on tick errors, never a panic. Each tick:
//!
//! 1. **Reclamation** — runs stuck in `running` for more than
//!    [`RUN_LEASE_TIMEOUT`] (worker crash / stall) are failed by the
//!    store's CAS-guarded `reclaim_stale_running`; a reclaimed run is
//!    retried through the normal requeue path and re-enters
//!    idempotently via the external-id match.
//! 2. **Post-apply sweep** (PR 6) — for every architecture whose
//!    config has `enable_post_apply = true`, enqueue a `post_apply`
//!    export run for the most recent `succeeded` apply run's version,
//!    unless one already exists for that version (idempotent across
//!    ticks) or an active run holds the architecture's one-active slot
//!    (coalescing). See [`Self::enqueue_post_apply_triggers`].
//! 3. **Claim** — one queued run per architecture per tick
//!    (`list_architecture_ids_with_queued` + `claim_next_queued`; the
//!    claim is a single atomic UPDATE, so concurrent workers cannot
//!    double-claim).
//! 4. **Execute** — load the config and decrypt the token in-memory
//!    (fail-closed), resolve the applied version, parse the
//!    `CHVArchitecture` model, and hand plain data to
//!    [`chv_netbox_adapter::NetboxProjectionRunner`]. The adapter stays
//!    store-free; this worker is the only place the two worlds meet.
//! 5. **Persist** — `mark_succeeded` with the serialized outcome and
//!    plan summary, or `mark_failed` with a redacted, secret-free
//!    error message (plus the per-entry outcome ledger when the run
//!    partially executed), plus an `EventType::Audit` event carrying
//!    the run/architecture ids and summary (never the token).
//!    Transient failures (NetBox unreachable, auth rejected, server
//!    5xx) are requeued for an automatic, exponentially backed-off
//!    retry bounded by the store's attempt cap; permanent failures
//!    stay `failed`.
//!
//! A NetBox outage never kills the worker: every error path marks the
//! run failed, emits an event, and continues to the next tick.

use std::sync::Arc;
use std::time::Duration;

use chv_architecture_validate::model::CHVArchitecture;
use chv_controlplane_store::{
    is_active_run_conflict, ApplyRunRepository, EventAppendInput, EventRepository,
    NetboxProjectionConfigRepository, NetboxProjectionRunCreateInput,
    NetboxProjectionRunRepository, VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureVersionId, NetboxProjectionMode, NetboxProjectionRun, NetboxProjectionRunId,
    NetboxProjectionTrigger, NetboxRetentionPolicy, RunStatus,
};
use chv_controlplane_types::domain::{EventSeverity, EventType};
use chv_errors::ChvError;
use chv_netbox_adapter::{
    ownership::CustomFieldNames, plan::RetentionPolicy, ClientError, NetBoxClient, NetBoxToken,
    NetboxProjectionInput, NetboxProjectionRunner,
};
use serde::Serialize;
use tracing::{debug, info, warn};

/// How long a `running` run may execute before reclamation (the
/// component spec's lease timeout; the client's 30s request timeout
/// bounds a single HTTP call, so 15 minutes covers a full plan with
/// generous headroom).
const RUN_LEASE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Upper bound on architectures processed per tick — keeps one tick
/// bounded even on fleets with many queued architectures.
const MAX_ARCHITECTURES_PER_TICK: usize = 32;

/// Audit event names (component spec "Security, RBAC and secrets"):
/// emitted as `EventType::Audit` with the event name as the message
/// and structured details (run id, architecture id, summary — never
/// the token).
pub const EVENT_NETBOX_EXPORT_SUCCEEDED: &str = "architecture_netbox_export_succeeded";
pub const EVENT_NETBOX_EXPORT_FAILED: &str = "architecture_netbox_export_failed";
pub const EVENT_NETBOX_DRY_RUN: &str = "architecture_netbox_dry_run";

/// Seam for tests: how the worker builds its NetBox client from the
/// stored config. Production uses [`NetBoxClient::new`] (HTTPS-only);
/// the wiremock suites inject the test-only unchecked constructor.
///
/// Public so the `netbox-demo` feature's
/// [`crate::netbox_demo::plain_http_client_factory`] (itself
/// feature-gated) can hand a factory to
/// [`NetboxProjectionWorker::with_client_factory`] from the binary
/// crate. This is a type alias only — it grants no new capability.
pub type ClientFactory =
    Arc<dyn Fn(&str, NetBoxToken) -> Result<NetBoxClient, ClientError> + Send + Sync>;

/// Background worker that claims queued NetBox projection runs and
/// executes them against the configured NetBox instance.
#[derive(Clone)]
pub struct NetboxProjectionWorker {
    run_repo: NetboxProjectionRunRepository,
    config_repo: NetboxProjectionConfigRepository,
    event_repo: EventRepository,
    apply_run_repo: ApplyRunRepository,
    version_repo: VersionRepository,
    tick_interval: Duration,
    client_factory: ClientFactory,
}

impl NetboxProjectionWorker {
    pub fn new(
        run_repo: NetboxProjectionRunRepository,
        config_repo: NetboxProjectionConfigRepository,
        event_repo: EventRepository,
        apply_run_repo: ApplyRunRepository,
        version_repo: VersionRepository,
    ) -> Self {
        Self {
            run_repo,
            config_repo,
            event_repo,
            apply_run_repo,
            version_repo,
            tick_interval: Duration::from_secs(30),
            client_factory: Arc::new(NetBoxClient::new),
        }
    }

    /// Replace the client-construction seam (tests only).
    pub fn with_client_factory(mut self, factory: ClientFactory) -> Self {
        self.client_factory = factory;
        self
    }

    pub async fn run(self, mut shutdown_rx: tokio::sync::watch::Receiver<()>) {
        info!("netbox projection worker starting");
        let mut interval = tokio::time::interval(self.tick_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown_rx.changed() => {
                    info!("netbox projection worker shutting down");
                    break;
                }
            }
            if let Err(e) = self.tick().await {
                warn!(error = %e, "netbox projection worker tick failed");
            }
        }
    }

    /// One worker iteration. Store-level failures bubble to `run`
    /// (warn + continue); per-run failures are handled inside
    /// `process_run` (mark failed + event) and never bubble.
    ///
    /// The post-apply sweep runs after reclamation but **before** the
    /// claim loop, so a just-succeeded apply enqueues its projection on
    /// the very same tick.
    pub async fn tick(&self) -> Result<(), ChvError> {
        self.reclaim_stale_runs().await?;
        self.enqueue_post_apply_triggers().await?;
        self.claim_and_process().await?;
        Ok(())
    }

    async fn reclaim_stale_runs(&self) -> Result<(), ChvError> {
        let before =
            chrono::Utc::now() - chrono::Duration::seconds(RUN_LEASE_TIMEOUT.as_secs() as i64);
        let reclaimed = self
            .run_repo
            .reclaim_stale_running(before)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to reclaim stale netbox projection runs: {e}"),
            })?;
        for run in reclaimed {
            warn!(
                run_id = %run.id,
                architecture_id = %run.architecture_id,
                "reclaimed stale netbox projection run (execution lease expired)"
            );
            self.emit_event(
                &run,
                EVENT_NETBOX_EXPORT_FAILED,
                EventSeverity::Error,
                &serde_json::json!({
                    "run_id": run.id.to_string(),
                    "architecture_id": run.architecture_id.to_string(),
                    "reason": "reclaimed",
                }),
            )
            .await;
            // A lease expiry means the worker died mid-run — a transient
            // failure class. Retry through the normal requeue path
            // (bounded by MAX_ATTEMPTS and the backoff schedule) so an
            // abandoned export does not require manual operator action.
            match self.run_repo.requeue(&run.id).await {
                Ok(requeued) => {
                    info!(
                        run_id = %run.id,
                        architecture_id = %run.architecture_id,
                        attempt_count = requeued.attempt_count,
                        next_attempt_at = ?requeued.next_attempt_at,
                        "reclaimed run requeued with backoff"
                    );
                }
                Err(e) => {
                    // Attempt cap exhausted or another run became active —
                    // the run stays failed; a normal bounded-retry outcome.
                    warn!(
                        run_id = %run.id,
                        architecture_id = %run.architecture_id,
                        error = %e,
                        "reclaimed run was not requeued (attempt cap or active run)"
                    );
                }
            }
        }
        Ok(())
    }

    /// Post-apply trigger sweep (PR 6 of the #239 plan).
    ///
    /// The plan's original wording placed this hook at the apply-run
    /// terminal transition site, but that site does not exist: the
    /// apply path defers the terminal transitions to the orchestrator
    /// (its own module doc — "The orchestrator (out of scope for
    /// Phase 5) is responsible for the terminal `Succeeded` /
    /// `PartiallyFailed` / `Failed` transitions; this module only puts
    /// the run on the rails" — and nothing in the codebase writes
    /// `RunStatus::Succeeded` to `architecture_apply_runs`). The
    /// trigger is therefore realized as a sweep over the durable
    /// outcome: for every architecture whose config has
    /// `enable_post_apply = true`, take the most recent `succeeded`
    /// apply run and enqueue a `queued` `post_apply` export run for
    /// its version.
    ///
    /// This is strictly more isolated than an in-line hook (the apply
    /// path calls nothing — a NetBox outage or a projection-store
    /// failure is structurally incapable of changing an apply result,
    /// the issue's headline AC) and behaviorally equivalent once the
    /// orchestrator's terminal transitions land: a fresh `succeeded`
    /// apply run fires here on the very next tick with zero changes.
    ///
    /// Semantics:
    ///
    /// - **Idempotent across ticks**: the enqueue is skipped when a
    ///   `post_apply` run of ANY status already exists for the
    ///   (architecture, version) pair (`has_post_apply_for_version`).
    ///   A permanently-failed post_apply run is therefore not
    ///   re-enqueued automatically — transient failures are owned by
    ///   the bounded auto-requeue, and after the attempt cap the
    ///   operator retries manually.
    /// - **Coalescing**: an active (queued/running) run — manual or
    ///   post_apply — holds the architecture's one-active slot, so the
    ///   create fails with the store's active-run conflict; that
    ///   outcome is a skip (the next export reconciles), not an error.
    /// - **Isolation**: a failure while handling one architecture
    ///   (store hiccup, corrupt row) is `warn!`-ed and the sweep moves
    ///   on to the next architecture; only a total failure of the
    ///   config listing bubbles to the tick loop's existing
    ///   warn-and-continue discipline.
    /// - **Enqueue-time snapshot**: the `architecture_version_id`
    ///   persisted on a post_apply run is a best-effort snapshot taken
    ///   at enqueue. Execution re-resolves the most recent `succeeded`
    ///   version per the PR-4 provenance rule, so a run enqueued for
    ///   v2 may project v3 if a newer apply succeeded in between — the
    ///   next sweep then enqueues a run for v3, whose execution is
    ///   idempotent (external-id match → no-op). Benign, and recorded
    ///   in the run's result envelope
    ///   (`resolved_architecture_version_id`).
    ///
    /// No audit event is emitted at enqueue: the API contract's event
    /// set (`architecture_netbox_export_succeeded` / `_failed`,
    /// `_dry_run`, `_retried`) is defined at execution time, and the
    /// manual export endpoint likewise emits nothing at enqueue — the
    /// run row itself is the enqueue record. A `debug!` covers
    /// observability.
    async fn enqueue_post_apply_triggers(&self) -> Result<(), ChvError> {
        let configs = self
            .config_repo
            .list_post_apply_enabled()
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to list post-apply-enabled netbox configs: {e}"),
            })?;

        for config in configs {
            // Apply-run listing: `list_for_architecture` orders
            // `created_at DESC, rowid DESC` — newest first, with
            // SQLite's insertion-ordered `rowid` breaking same-second
            // ties — and does not filter by status, so the first
            // `Succeeded` row here is the most recent successful
            // apply.
            let apply_runs = match self
                .apply_run_repo
                .list_for_architecture(&config.architecture_id, None)
                .await
            {
                Ok(runs) => runs,
                Err(e) => {
                    warn!(
                        architecture_id = %config.architecture_id,
                        error = %e,
                        "post-apply sweep: apply-run listing failed; skipping architecture"
                    );
                    continue;
                }
            };
            let Some(latest_succeeded) = apply_runs
                .iter()
                .find(|apply| apply.status == RunStatus::Succeeded)
            else {
                debug!(
                    architecture_id = %config.architecture_id,
                    "post-apply sweep: no succeeded apply run; skipping architecture"
                );
                continue;
            };
            let version_id = latest_succeeded.architecture_version_id.clone();

            match self
                .run_repo
                .has_post_apply_for_version(&config.architecture_id, &version_id)
                .await
            {
                Ok(true) => {
                    debug!(
                        architecture_id = %config.architecture_id,
                        architecture_version_id = %version_id,
                        "post-apply sweep: run already exists for this version; skipping"
                    );
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        architecture_id = %config.architecture_id,
                        architecture_version_id = %version_id,
                        error = %e,
                        "post-apply sweep: post-apply lookup failed; skipping architecture"
                    );
                    continue;
                }
            }

            let run_id = match NetboxProjectionRunId::new(chv_common::gen_resource_id()) {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        architecture_id = %config.architecture_id,
                        error = %e,
                        "post-apply sweep: run id generation failed; skipping architecture"
                    );
                    continue;
                }
            };
            match self
                .run_repo
                .create(NetboxProjectionRunCreateInput {
                    id: run_id,
                    architecture_id: config.architecture_id.clone(),
                    architecture_version_id: version_id.clone(),
                    trigger_kind: NetboxProjectionTrigger::PostApply,
                    mode: NetboxProjectionMode::Export,
                    plan_json: None,
                    // System trigger: no human requested this run.
                    requested_by: None,
                })
                .await
            {
                Ok(run) => {
                    info!(
                        run_id = %run.id,
                        architecture_id = %run.architecture_id,
                        architecture_version_id = %run.architecture_version_id,
                        "post-apply trigger enqueued netbox projection run"
                    );
                }
                // The one-active partial index rejected the insert: an
                // active run (manual or post_apply) already holds the
                // architecture's slot — coalesce per the plan.
                Err(e) if is_active_run_conflict(&e) => {
                    debug!(
                        architecture_id = %config.architecture_id,
                        architecture_version_id = %version_id,
                        "post-apply sweep: active run exists; coalescing"
                    );
                }
                Err(e) => {
                    warn!(
                        architecture_id = %config.architecture_id,
                        architecture_version_id = %version_id,
                        error = %e,
                        "post-apply sweep: enqueue failed; skipping architecture"
                    );
                }
            }
        }
        Ok(())
    }

    async fn claim_and_process(&self) -> Result<(), ChvError> {
        let architecture_ids = self
            .run_repo
            .list_architecture_ids_with_queued()
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to list architectures with queued netbox runs: {e}"),
            })?;

        for architecture_id in architecture_ids
            .into_iter()
            .take(MAX_ARCHITECTURES_PER_TICK)
        {
            // One claim per architecture per tick; the claim is an
            // atomic queued→running UPDATE, so two workers (or two
            // ticks) can never double-claim the same run.
            let Some(run) = self
                .run_repo
                .claim_next_queued(&architecture_id)
                .await
                .map_err(|e| ChvError::Internal {
                    reason: format!("failed to claim netbox projection run: {e}"),
                })?
            else {
                continue;
            };
            if let Err(e) = self.process_run(&run).await {
                warn!(
                    run_id = %run.id,
                    architecture_id = %run.architecture_id,
                    error = %e,
                    "failed to process netbox projection run"
                );
            }
        }
        Ok(())
    }

    /// Execute one claimed run end to end. Every failure path marks the
    /// run failed and emits an audit event; nothing panics and nothing
    /// propagates to the worker loop.
    async fn process_run(&self, run: &NetboxProjectionRun) -> Result<(), ChvError> {
        // Config: absent → NETBOX_NOT_CONFIGURED (the post-apply trigger
        // coalesces instead of enqueueing, but a config deleted between
        // enqueue and execution still lands here).
        let Some(config) = self
            .config_repo
            .get(&run.architecture_id)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to load netbox projection config: {e}"),
            })?
        else {
            return self
                .fail_run(
                    run,
                    "netbox not configured for this architecture (NETBOX_NOT_CONFIGURED)",
                    None,
                    false,
                )
                .await;
        };

        // Token: decrypt in-memory only, fail-closed. The plaintext
        // never leaves this function.
        let token = match self.config_repo.read_token(&run.architecture_id).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                return self
                    .fail_run(
                        run,
                        "netbox token missing for this architecture (NETBOX_TOKEN_MISSING)",
                        None,
                        false,
                    )
                    .await;
            }
            Err(e) => {
                // Fail-closed: the store never returns the ciphertext,
                // and neither does this message.
                warn!(
                    architecture_id = %run.architecture_id,
                    error = %e,
                    "netbox token decrypt failed (fail-closed)"
                );
                return self
                    .fail_run(
                        run,
                        "netbox token unreadable for this architecture (NETBOX_TOKEN_MISSING)",
                        None,
                        false,
                    )
                    .await;
            }
        };

        // Applied-version gate (the projection source): the most
        // recent `succeeded` apply run's version — never the editable
        // draft, and never blindly the run row's enqueued version id
        // (a newer apply may have succeeded between enqueue and
        // execution). `list_for_architecture` orders
        // `created_at DESC, rowid DESC` — newest first, with SQLite's
        // insertion-ordered `rowid` breaking same-second ties — and
        // does not filter by status, so the first `Succeeded` row
        // here is the most recent successful apply.
        let apply_runs = self
            .apply_run_repo
            .list_for_architecture(&run.architecture_id, None)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to list apply runs: {e}"),
            })?;
        let Some(latest_succeeded) = apply_runs
            .iter()
            .find(|apply| apply.status == RunStatus::Succeeded)
        else {
            return self
                .fail_run(
                    run,
                    "architecture has no succeeded apply run (NETBOX_NOT_APPLIED)",
                    None,
                    false,
                )
                .await;
        };
        let resolved_version_id = latest_succeeded.architecture_version_id.clone();
        if resolved_version_id != run.architecture_version_id {
            // The enqueue-time snapshot is stale: project the version
            // that is actually applied. Provenance is recorded in the
            // result envelope (`resolved_architecture_version_id`).
            debug!(
                run_id = %run.id,
                architecture_id = %run.architecture_id,
                enqueued_architecture_version_id = %run.architecture_version_id,
                resolved_architecture_version_id = %resolved_version_id,
                "projection source resolved to the most recent succeeded apply version"
            );
        }

        // Version row: a missing or unreadable row fails THIS run
        // inline — it must never propagate out of the run-processing
        // path (a propagated error is only `warn!`ed by the claim loop
        // and would leave the run stuck `running` until lease
        // reclamation).
        let version = match self.version_repo.get(&resolved_version_id, None).await {
            Ok(version) => version,
            Err(e) => {
                warn!(
                    run_id = %run.id,
                    architecture_id = %run.architecture_id,
                    error = %e,
                    "architecture version row could not be loaded"
                );
                return self
                    .fail_run(
                        run,
                        "architecture version row missing for the applied version (NETBOX_NOT_APPLIED)",
                        None,
                        false,
                    )
                    .await;
            }
        };
        let Some(model_json) = version.normalized_model_json.as_deref() else {
            return self
                .fail_run(
                    run,
                    "applied architecture version carries no normalized model",
                    None,
                    false,
                )
                .await;
        };
        let architecture: CHVArchitecture = match serde_json::from_str(model_json) {
            Ok(model) => model,
            Err(e) => {
                return self
                    .fail_run(
                        run,
                        &format!("applied architecture model could not be parsed: {e}"),
                        None,
                        false,
                    )
                    .await;
            }
        };

        // Client: HTTPS enforced by the constructor (fail-closed).
        let client = match (self.client_factory)(&config.endpoint, NetBoxToken::new(token.clone()))
        {
            Ok(client) => client,
            Err(e) => {
                return self
                    .fail_run(run, &redact(&e.to_string(), &token), None, false)
                    .await;
            }
        };
        let runner = NetboxProjectionRunner::new(client);

        let input = NetboxProjectionInput {
            architecture: &architecture,
            architecture_id: run.architecture_id.as_str(),
            architecture_version: version.version_number as u64,
            snapshot: None,
            site_name: config.site_name.as_deref(),
            retention: match config.retention_policy {
                NetboxRetentionPolicy::MarkStale => RetentionPolicy::MarkStale,
                NetboxRetentionPolicy::Delete => RetentionPolicy::Delete,
            },
            names: CustomFieldNames::new(&config.custom_field_prefix),
        };

        match run.mode {
            NetboxProjectionMode::DryRun => match runner.dry_run(&input).await {
                Ok(plan) => {
                    let result_json = Some(self.result_envelope(&resolved_version_id, &plan)?);
                    let summary_json = serde_json::to_string(&plan.summary).ok();
                    self.finish_run(run, &plan.summary, result_json, summary_json)
                        .await
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    self.fail_run(run, &redact(&e.to_string(), &token), None, retryable)
                        .await
                }
            },
            NetboxProjectionMode::Export => match runner.run(&input).await {
                Ok(outcome) => {
                    // The outcome is persisted in BOTH terminal states:
                    // on partial failure the per-entry ledger records
                    // which entries succeeded / failed / were not
                    // attempted, so the executed work stays inspectable
                    // and resumable via the external-id match on retry.
                    let result_json = Some(self.result_envelope(&resolved_version_id, &outcome)?);
                    if let Some(error) = &outcome.error {
                        let message = redact(&error.message, &token);
                        let retryable = error.retryable;
                        return self.fail_run(run, &message, result_json, retryable).await;
                    }
                    let summary_json = serde_json::to_string(&outcome.plan.summary).ok();
                    self.finish_run(run, &outcome.plan.summary, result_json, summary_json)
                        .await
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    self.fail_run(run, &redact(&e.to_string(), &token), None, retryable)
                        .await
                }
            },
        }
    }

    /// Serialize a run result inside its provenance envelope: the
    /// architecture version that was actually projected
    /// (`resolved_architecture_version_id` — the most recent succeeded
    /// apply run's version, see `process_run`) alongside the plan or
    /// outcome itself. BTree-ordered and secret-free by construction.
    fn result_envelope(
        &self,
        resolved_version_id: &ArchitectureVersionId,
        result: &impl Serialize,
    ) -> Result<String, ChvError> {
        serde_json::to_string(&serde_json::json!({
            "resolved_architecture_version_id": resolved_version_id.as_str(),
            "result": result,
        }))
        .map_err(|e| ChvError::Internal {
            reason: format!("failed to serialize netbox projection result: {e}"),
        })
    }

    /// Terminal `running → succeeded` + success event.
    async fn finish_run(
        &self,
        run: &NetboxProjectionRun,
        summary: &chv_netbox_adapter::PlanSummary,
        result_json: Option<String>,
        summary_json: Option<String>,
    ) -> Result<(), ChvError> {
        self.run_repo
            .mark_succeeded(&run.id, result_json, summary_json)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark netbox projection run succeeded: {e}"),
            })?;
        info!(
            run_id = %run.id,
            architecture_id = %run.architecture_id,
            create = summary.create,
            update = summary.update,
            no_op = summary.no_op,
            conflict = summary.conflict,
            stale = summary.stale,
            "netbox projection run succeeded"
        );
        let (event_name, severity) = match run.mode {
            NetboxProjectionMode::Export => (EVENT_NETBOX_EXPORT_SUCCEEDED, EventSeverity::Info),
            NetboxProjectionMode::DryRun => (EVENT_NETBOX_DRY_RUN, EventSeverity::Info),
        };
        self.emit_event(
            run,
            event_name,
            severity,
            &event_details(run, &serde_json::to_value(summary).unwrap_or_default()),
        )
        .await;
        Ok(())
    }

    /// Terminal `running → failed` + failure event. `message` must
    /// already be redacted by the caller. `result_json` optionally
    /// persists the per-entry outcome ledger of a partially executed
    /// run.
    ///
    /// **Bounded auto-retry**: when the failure class is transient
    /// (`retryable` — NetBox unreachable, auth rejected mid-run, or a
    /// server-side 5xx) the run is requeued after being marked failed,
    /// with the store's exponential backoff gating the next claim. The
    /// requeued row keeps the failure history (`error_message` /
    /// `result_json`) until the retry overwrites it. Permanent
    /// failures — not configured, not applied, bad config, contract or
    /// model violations — stay `failed` for operator inspection.
    /// Retrying is additionally bounded by the store's `MAX_ATTEMPTS`
    /// cap and the one-active-run index; a requeue refused for either
    /// reason is logged and leaves the run failed.
    async fn fail_run(
        &self,
        run: &NetboxProjectionRun,
        message: &str,
        result_json: Option<String>,
        retryable: bool,
    ) -> Result<(), ChvError> {
        self.run_repo
            .mark_failed(&run.id, Some(message.to_string()), result_json)
            .await
            .map_err(|e| ChvError::Internal {
                reason: format!("failed to mark netbox projection run failed: {e}"),
            })?;
        warn!(
            run_id = %run.id,
            architecture_id = %run.architecture_id,
            error = %message,
            retryable,
            "netbox projection run failed"
        );
        let (event_name, severity) = match run.mode {
            NetboxProjectionMode::Export => (EVENT_NETBOX_EXPORT_FAILED, EventSeverity::Error),
            // A dry run that cannot reach NetBox reports under the
            // dry-run event with error severity — it never wrote.
            NetboxProjectionMode::DryRun => (EVENT_NETBOX_DRY_RUN, EventSeverity::Error),
        };
        self.emit_event(
            run,
            event_name,
            severity,
            &serde_json::json!({
                "run_id": run.id.to_string(),
                "architecture_id": run.architecture_id.to_string(),
                "error": message,
                "retryable": retryable,
            }),
        )
        .await;
        if retryable {
            match self.run_repo.requeue(&run.id).await {
                Ok(requeued) => {
                    info!(
                        run_id = %run.id,
                        architecture_id = %run.architecture_id,
                        attempt_count = requeued.attempt_count,
                        next_attempt_at = ?requeued.next_attempt_at,
                        "transient netbox failure; run requeued with backoff"
                    );
                }
                Err(e) => {
                    // Attempt cap exhausted or another run became
                    // active — the run stays failed; that is a normal
                    // bounded-retry outcome, not a worker error.
                    warn!(
                        run_id = %run.id,
                        architecture_id = %run.architecture_id,
                        error = %e,
                        "failed run was not requeued (attempt cap or active run)"
                    );
                }
            }
        }
        Ok(())
    }

    /// Append the audit event. Best-effort: an event-store failure is
    /// logged and swallowed — the run's terminal state is the source of
    /// truth, and a broken events table must not fail the run twice.
    async fn emit_event(
        &self,
        run: &NetboxProjectionRun,
        event_name: &str,
        severity: EventSeverity,
        details: &serde_json::Value,
    ) {
        let input = EventAppendInput {
            occurred_unix_ms: chrono::Utc::now().timestamp_millis(),
            event_type: EventType::Audit,
            severity,
            resource_kind: None,
            resource_id: None,
            node_id: None,
            operation_id: None,
            actor_id: None,
            requested_by: run.requested_by.clone(),
            correlation_id: Some(run.id.to_string()),
            message: event_name.to_string(),
            details: Some(details.to_string()),
        };
        if let Err(e) = self.event_repo.append(&input).await {
            warn!(
                run_id = %run.id,
                error = %e,
                "failed to append netbox projection audit event"
            );
        }
    }
}

/// Structured, BTree-ordered event details (secret-free by
/// construction: summary counts only).
fn event_details(run: &NetboxProjectionRun, summary: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "run_id": run.id.to_string(),
        "architecture_id": run.architecture_id.to_string(),
        "mode": run.mode.as_str(),
        "trigger": run.trigger_kind.as_str(),
        "summary": summary,
    })
}

/// Belt-and-braces redaction: scrub any occurrence of the plaintext
/// token from a message before it is persisted or logged. Error
/// displays are token-free by construction; this catches regressions.
fn redact(message: &str, token: &str) -> String {
    if token.is_empty() {
        message.to_string()
    } else {
        message.replace(token, "<redacted>")
    }
}
