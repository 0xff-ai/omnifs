ALTER TABLE credentials
ADD COLUMN action_generation INTEGER NOT NULL DEFAULT 0
CHECK (action_generation >= 0);

CREATE TABLE action_receipts (
    action_id BLOB PRIMARY KEY CHECK (length(action_id) = 16),
    kind TEXT NOT NULL CHECK (
        kind IN (
            'set-credential-material',
            'revoke-credential',
            'restart-attachment'
        )
    ),
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('credential', 'attachment')
    ),
    target_name TEXT NOT NULL CHECK (
        length(target_name) BETWEEN 1 AND 32
        AND substr(target_name, 1, 1) GLOB '[a-z0-9]'
        AND target_name NOT GLOB '*[^a-z0-9-]*'
    ),
    request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
    action_generation INTEGER NOT NULL CHECK (action_generation > 0),
    phase TEXT NOT NULL CHECK (
        phase IN ('accepted', 'running', 'retrying', 'ready', 'failed')
    ),
    error_code TEXT,
    detail TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (
        (phase = 'failed' AND error_code IS NOT NULL)
        OR (phase <> 'failed' AND error_code IS NULL)
    )
) STRICT;

CREATE UNIQUE INDEX action_receipts_one_pending_target_idx
    ON action_receipts(target_kind, target_name)
    WHERE phase IN ('accepted', 'running', 'retrying');

CREATE INDEX action_receipts_created_at_idx
    ON action_receipts(created_at);
