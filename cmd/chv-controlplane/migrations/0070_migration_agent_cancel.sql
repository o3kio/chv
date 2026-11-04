-- Agent-side migration-task cancellation (#656).
--
-- `MigrateVm` is fire-and-forget: the source agent ACKs immediately and runs
-- the disk/memory migration on a detached task (the destination agent does the
-- same for its receive task). The control-plane state machine can disappear
-- (restart) or terminalize a row (reaper, operator cancel, rollback) while
-- those tasks are still alive. Since there was no abort-migration RPC, the
-- orphans kept writing to the destination and could pause/resume the source VM
-- with no control-plane owner.
--
-- `agent_cancel_pending` marks a row whose detached agent tasks must be
-- signalled via the MigrationControlService.CancelMigration RPC. The migration
-- reaper drains the flag on its next tick: a successful response (including
-- `not_tracked`, meaning the agent restarted and the task is already gone)
-- clears the flag and records `agent_cancel_confirmed_at`; a transport
-- failure leaves the flag set for the next tick. The attempt columns make
-- draining fair: a persistently unreachable node moves to the back of the
-- queue instead of starving newer rows, and `attempts` surfaces the condition
-- in logs/metrics. `agent_cancel_last_error` parks a cancel that the agent
-- refused permanently (core-managed or pre-#656 agent) so it cannot retry
-- forever.
--
-- The columns are additive, so rows written by older binaries keep
-- `0`/NULL and are never scanned.
ALTER TABLE migrations ADD COLUMN agent_cancel_pending INTEGER NOT NULL DEFAULT 0;
ALTER TABLE migrations ADD COLUMN agent_cancel_confirmed_at TEXT;
ALTER TABLE migrations ADD COLUMN agent_cancel_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE migrations ADD COLUMN agent_cancel_last_attempt_at TEXT;
ALTER TABLE migrations ADD COLUMN agent_cancel_last_error TEXT;

-- Backfill the orphans that motivated #656. A pre-upgrade binary terminalized
-- these rows without queueing a cancel, so the old code would never signal
-- their surviving agent tasks. Only recent failures are marked: agent-side
-- tasks cannot have survived for weeks, and over-marking self-drains through
-- `not_tracked` on the first reaper tick.
UPDATE migrations
   SET agent_cancel_pending = 1
 WHERE phase IN ('Failed', 'RolledBack')
   AND operation_id <> ''
   AND completed_at IS NOT NULL
   AND completed_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days');
