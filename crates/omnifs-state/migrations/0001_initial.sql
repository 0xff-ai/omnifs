CREATE TABLE providers (
    digest BLOB PRIMARY KEY CHECK (length(digest) = 32),
    name TEXT NOT NULL,
    version TEXT,
    metadata BLOB NOT NULL,
    wasm BLOB NOT NULL,
    wasm_length INTEGER NOT NULL CHECK (wasm_length >= 0),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE credentials (
    provider_name TEXT NOT NULL,
    provider_digest BLOB NOT NULL CHECK (length(provider_digest) = 32),
    scheme TEXT NOT NULL,
    account TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('static-token', 'oauth')),
    material BLOB NOT NULL,
    auth_fingerprint BLOB NOT NULL CHECK (length(auth_fingerprint) = 32),
    version INTEGER NOT NULL CHECK (version > 0),
    generation INTEGER NOT NULL CHECK (generation > 0),
    action_generation INTEGER NOT NULL DEFAULT 0 CHECK (action_generation >= 0),
    status TEXT NOT NULL CHECK (
        status IN (
            'active',
            'blocked',
            'pending-republish',
            'revocation-pending',
            'revocation-unknown',
            'deleted'
        )
    ),
    revocation_intent BLOB,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_name, scheme, account),
    FOREIGN KEY (provider_digest) REFERENCES providers(digest)
) STRICT;

CREATE TABLE recovery_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    state TEXT NOT NULL CHECK (state IN ('ready', 'recovery-required')),
    detail TEXT,
    serving_resource_revision INTEGER NOT NULL CHECK (serving_resource_revision >= 0),
    updated_at INTEGER NOT NULL
) STRICT;

INSERT INTO recovery_state(
    singleton,
    state,
    detail,
    serving_resource_revision,
    updated_at
)
VALUES (1, 'ready', NULL, 0, unixepoch());

CREATE TABLE attach_endpoint (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    tcp_port INTEGER NOT NULL CHECK (tcp_port BETWEEN 1 AND 65535)
) STRICT;

CREATE TABLE resource_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    desired_digest BLOB NOT NULL CHECK (length(desired_digest) = 32),
    resources BLOB NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- Empty normalized desired set and its BLAKE3 digest.
INSERT INTO resource_state(
    singleton,
    revision,
    desired_digest,
    resources,
    updated_at
)
VALUES (
    1,
    0,
    X'adc28defe5460afa3015496b2cd982a5f018e9b66f3b0aca5294a2a0936dafdd',
    X'6f6d6e6966732e7265736f75726365732e76310000',
    unixepoch()
);

CREATE TABLE apply_receipts (
    mutation_id BLOB PRIMARY KEY CHECK (length(mutation_id) = 16),
    input_digest BLOB NOT NULL CHECK (length(input_digest) = 32),
    result_revision INTEGER NOT NULL CHECK (result_revision >= 0),
    result_digest BLOB NOT NULL CHECK (length(result_digest) = 32),
    changed INTEGER NOT NULL CHECK (changed IN (0, 1)),
    created INTEGER NOT NULL CHECK (created >= 0),
    updated INTEGER NOT NULL CHECK (updated >= 0),
    deleted INTEGER NOT NULL CHECK (deleted >= 0),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX apply_receipts_created_at_idx
    ON apply_receipts(created_at);

CREATE TABLE action_receipts (
    action_id BLOB PRIMARY KEY CHECK (length(action_id) = 16),
    kind TEXT NOT NULL CHECK (
        kind IN (
            'set-credential-material',
            'revoke-credential',
            'restart-filesystem'
        )
    ),
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('credential', 'filesystem')
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

CREATE TABLE filesystem_instances (
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

CREATE INDEX filesystem_instances_phase_idx
    ON filesystem_instances(phase, updated_at);
