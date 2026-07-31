-- Observed lifecycle state for daemon-owned attachment runtimes.  Rows are
-- independent from attachment_resources so a deleting tombstone can survive
-- a desired-resource update and a daemon restart.
CREATE TABLE attachment_instances (
    name TEXT PRIMARY KEY CHECK (
        length(name) BETWEEN 1 AND 32
        AND substr(name, 1, 1) GLOB '[a-z0-9]'
        AND name NOT GLOB '*[^a-z0-9-]*'
    ),
    desired_version BLOB
        CHECK (desired_version IS NULL OR length(desired_version) = 32),
    desired_spec BLOB,
    observed_version BLOB
        CHECK (observed_version IS NULL OR length(observed_version) = 32),
    observed_spec BLOB,
    phase TEXT NOT NULL CHECK (
        phase IN (
            'pending',
            'waiting_for_namespace',
            'starting',
            'ready',
            'stopping',
            'retrying',
            'failed',
            'deleting'
        )
    ),
    runtime_instance TEXT
        CHECK (
            runtime_instance IS NULL
            OR (
                length(runtime_instance) = 32
                AND runtime_instance NOT GLOB '*[^0-9a-f]*'
            )
        ),
    action_generation INTEGER NOT NULL CHECK (action_generation >= 0),
    last_error_code TEXT,
    last_error_detail TEXT,
    retry_at INTEGER CHECK (retry_at IS NULL OR retry_at >= 0),
    deleting INTEGER NOT NULL CHECK (deleting IN (0, 1)),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    CHECK ((desired_version IS NULL) = (desired_spec IS NULL)),
    CHECK ((observed_version IS NULL) = (observed_spec IS NULL))
) STRICT;

CREATE INDEX attachment_instances_phase_idx
    ON attachment_instances(phase, updated_at);

-- Existing desired attachments become pending work for the first daemon that
-- opens this migration. Their exact canonical spec stays available for
-- recovery before any runtime starts.
INSERT INTO attachment_instances(
    name,
    desired_version,
    desired_spec,
    observed_version,
    observed_spec,
    phase,
    runtime_instance,
    action_generation,
    last_error_code,
    last_error_detail,
    retry_at,
    deleting,
    updated_at
)
SELECT
    name,
    version,
    canonical,
    NULL,
    NULL,
    'pending',
    NULL,
    0,
    NULL,
    NULL,
    NULL,
    0,
    unixepoch()
FROM attachment_resources;
