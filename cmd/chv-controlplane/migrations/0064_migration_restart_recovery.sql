-- Restart-recovery bookkeeping for tracked migrations (#656).
--
-- The `migrations` table is the durable task record; the migration state
-- machine itself is an in-process future. When the control plane exits while
-- a migration is in flight, the future is gone but the row is still
-- non-terminal. At the next startup the registry reconciles those rows so an
-- operator can tell exactly what was interrupted and why.
--
-- `interrupted_from_phase` preserves the phase the row was in when the restart
-- was observed (the terminal `phase` overwrites it during reconciliation).
-- `interrupted_at` is the reconciliation timestamp.
--
-- Both columns are additive and nullable: rows written by older control
-- planes, and terminal rows that were never interrupted, keep NULL. Code that
-- predates this migration ignores them, so rollback to an older binary is safe
-- (SQLite migrations are forward-only; dropping the columns is not required).
ALTER TABLE migrations ADD COLUMN interrupted_from_phase TEXT;
ALTER TABLE migrations ADD COLUMN interrupted_at TEXT;
