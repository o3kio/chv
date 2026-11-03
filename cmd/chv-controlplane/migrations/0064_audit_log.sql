-- Durable audit log for privileged BFF mutations (#658).
--
-- The audit trail is append-only from the application's perspective:
-- the BFF inserts exactly one row per mutating request (success,
-- failure, or role/auth denial) and the only bulk delete is the
-- retention sweep. There is deliberately no foreign key to `users`:
-- an audit record must outlive the account it names, otherwise
-- deleting a user would erase the evidence of privileged actions
-- taken by that user.
--
-- Redaction contract: `metadata` is a JSON object built from an
-- explicit allowlist in the BFF (auth mechanism classification only).
-- Request bodies, query strings, Authorization/Cookie headers,
-- passwords, API tokens, and S3 keys are never read into it.
CREATE TABLE IF NOT EXISTS audit_log (
    audit_id TEXT PRIMARY KEY,
    occurred_at_ms INTEGER NOT NULL,
    actor_id TEXT NOT NULL,
    actor_username TEXT NOT NULL,
    actor_role TEXT NOT NULL,
    action TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    resource_kind TEXT,
    resource_id TEXT,
    -- success | failed | denied
    outcome TEXT NOT NULL,
    status_code INTEGER NOT NULL,
    correlation_id TEXT,
    operation_id TEXT,
    source_ip TEXT,
    -- Allowlisted, already-redacted JSON object (never a request body).
    metadata TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS audit_log_occurred_idx
    ON audit_log (occurred_at_ms DESC, audit_id DESC);
CREATE INDEX IF NOT EXISTS audit_log_actor_idx
    ON audit_log (actor_id, occurred_at_ms DESC);
CREATE INDEX IF NOT EXISTS audit_log_username_idx
    ON audit_log (actor_username, occurred_at_ms DESC);
CREATE INDEX IF NOT EXISTS audit_log_action_idx
    ON audit_log (action, occurred_at_ms DESC);
CREATE INDEX IF NOT EXISTS audit_log_resource_idx
    ON audit_log (resource_kind, resource_id, occurred_at_ms DESC);
CREATE INDEX IF NOT EXISTS audit_log_outcome_idx
    ON audit_log (outcome, occurred_at_ms DESC);
