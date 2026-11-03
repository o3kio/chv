-- Index the generation columns the BFF reads to seed its monotonic
-- generation floor (#664). Without these, each mint scans the whole
-- desired-state or operations table for MAX(desired_generation).
--
-- vm_desired_state already has idx_vm_desired_state_generation from
-- 0030_add_missing_indexes.sql.

CREATE INDEX IF NOT EXISTS idx_volume_desired_state_generation
    ON volume_desired_state(desired_generation);

CREATE INDEX IF NOT EXISTS idx_network_desired_state_generation
    ON network_desired_state(desired_generation);

CREATE INDEX IF NOT EXISTS idx_node_desired_state_generation
    ON node_desired_state(desired_generation);

CREATE INDEX IF NOT EXISTS idx_operations_desired_generation
    ON operations(desired_generation);
