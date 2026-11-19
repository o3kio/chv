-- Transactional outbox for the privileged-mutation audit trail (#706 half 1).
-- Rows are enqueued INSIDE the mutation's transaction (audit_outbox::
-- enqueue_on_tx), so a crash between the mutation commit and audit emission
-- can no longer lose the row: the outbox row commits with the mutation or
-- not at all. A bounded background dispatcher converts pending rows into
-- audit_log rows and marks them dispatched in the SAME transaction —
-- exactly-once per outbox row, no dead-letter (evidence is never dropped).
--
-- payload is the already-redacted AuditLogInsert as JSON, built by the BFF
-- middleware or the gRPC adapter from the same allowlists as the direct
-- path (audit-log.md redaction contract). The dispatcher never re-derives
-- content; retention/pruning of dispatched rows is plumbing cleanup, not
-- evidence deletion (audit_log remains the evidence table).
--
-- This migration is additive only: no backfill, no change to audit_log.
-- An empty outbox is a valid steady state; the direct-insert fallback path
-- keeps working unchanged.
CREATE TABLE IF NOT EXISTS audit_outbox (
    -- Idempotency key: minted once per mutating request by the emitter.
    outbox_id TEXT PRIMARY KEY,
    -- Final audit_log PK, minted at enqueue so dispatch is deterministic.
    audit_id TEXT NOT NULL UNIQUE,
    -- Redacted AuditLogInsert JSON (same field contract as audit_log).
    payload TEXT NOT NULL,
    -- pending | dispatched. No 'dead': audit evidence is never droppable.
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','dispatched')),
    attempts INTEGER NOT NULL DEFAULT 0,
    -- Due-time doubles as the claim lease (notification_outbox precedent).
    next_attempt_at_ms INTEGER NOT NULL,
    last_attempt_ms INTEGER,
    last_error TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    dispatched_at_ms INTEGER
);

CREATE INDEX IF NOT EXISTS audit_outbox_due_idx
    ON audit_outbox (status, next_attempt_at_ms);
-- Retention pruning of dispatched rows (bounded batches, oldest first).
CREATE INDEX IF NOT EXISTS audit_outbox_dispatched_idx
    ON audit_outbox (status, dispatched_at_ms);
