-- Migration reaper resume bookkeeping (#652).
--
-- The reaper fails a stalled migration, then tries to resume the source VM.
-- The resume can be legitimately refused (sender still inside the quiescence
-- window, source node not heartbeating yet) or fail transiently (node client
-- connect error). Before this table state, all of those outcomes were final:
-- the row was already terminal, so the next scan never revisited it and the
-- VM stayed paused until an operator intervened.
--
-- `reaper_resume_state` makes the resume durable:
--   NULL       — not a reaper-failed migration
--   'pending'  — resume not yet completed; retried every reaper tick
--   'resumed'  — resume RPC succeeded
--   'cancelled'— the phase re-check found a non-resumable phase (usually a
--                progress race; also covers a row that completed before the
--                resume pass looked again)
--   'missing'  — the migration row disappeared
--
-- `reaper_stalled_secs` preserves the no-progress age measured before
-- `fail_migration` rewrote `updated_at`. The retry pass adds the time elapsed
-- since that reap timestamp, so the effective silence grows and can eventually
-- satisfy the quiescence floor instead of staying frozen below it.
ALTER TABLE migrations ADD COLUMN reaper_resume_state TEXT;
ALTER TABLE migrations ADD COLUMN reaper_stalled_secs INTEGER;
ALTER TABLE migrations ADD COLUMN reaper_resume_attempts INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_migrations_reaper_resume
    ON migrations(reaper_resume_state)
    WHERE reaper_resume_state = 'pending';
