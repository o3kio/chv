-- Node certificate revocation denylist (#661).
--
-- A stolen/host-compromised node certificate stays cryptographically valid
-- until its not-after time. The node CA has no CRL/OCSP endpoint in v1
-- (ADR-029 §3.5), so the control plane keeps a durable denylist keyed by the
-- certificate serial it already records in `nodes.certificate_serial`.
--
-- Enforcement is at the gRPC peer-identity interceptor: every production
-- mTLS request is checked against an in-memory snapshot of this table before
-- the handler runs. The table is the source of truth; the snapshot is
-- refreshed on an interval and on operator revocation.
--
-- `expires_at` is optional bookkeeping for pruning: it records the revoked
-- certificate's own not-after time so an operator can subsequently delete
-- rows that can no longer authenticate anyway. It is deliberately NOT an
-- enforcement horizon -- a row whose expires_at has passed still denies the
-- serial. Fail closed: a clock problem or an early expiry value must never
-- resurrect a revoked credential.
--
-- Rows are never updated in place. The first revocation is authoritative
-- (reason, actor, timestamp); repeated revocations are idempotent no-ops so
-- an operator or retrying automation can safely re-run the command.
CREATE TABLE IF NOT EXISTS node_certificate_revocations (
    certificate_serial TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    revoked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    revoked_by TEXT,
    expires_at TEXT
);

-- Operator lookups: "what is revoked for this node?" and periodic
-- snapshot/pruning scans ordered by revocation time.
CREATE INDEX IF NOT EXISTS idx_node_certificate_revocations_node_id
    ON node_certificate_revocations (node_id);
CREATE INDEX IF NOT EXISTS idx_node_certificate_revocations_revoked_at
    ON node_certificate_revocations (revoked_at);
