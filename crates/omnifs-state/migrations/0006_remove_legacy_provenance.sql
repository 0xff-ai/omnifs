-- Resource receipts and durable action receipts now own retry provenance.
-- The old imperative control plane stamped these rows directly.
ALTER TABLE mounts DROP COLUMN last_mutation_id;
ALTER TABLE credentials DROP COLUMN last_mutation_id;
ALTER TABLE recovery_state DROP COLUMN failed_mutation_id;
