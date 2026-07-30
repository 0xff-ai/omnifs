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
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_name, scheme, account),
    FOREIGN KEY (provider_digest) REFERENCES providers(digest)
) STRICT;

CREATE TABLE mount_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;

INSERT INTO mount_state(singleton, revision) VALUES (1, 0);

CREATE TABLE mounts (
    name TEXT PRIMARY KEY,
    canonical BLOB NOT NULL,
    version BLOB NOT NULL CHECK (length(version) = 32),
    revision INTEGER NOT NULL CHECK (revision > 0),
    provider_digest BLOB NOT NULL,
    credential_provider_name TEXT,
    credential_scheme TEXT,
    credential_account TEXT,
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (provider_digest) REFERENCES providers(digest),
    CHECK (
        (credential_provider_name IS NULL
            AND credential_scheme IS NULL
            AND credential_account IS NULL)
        OR
        (credential_provider_name IS NOT NULL
            AND credential_scheme IS NOT NULL
            AND credential_account IS NOT NULL)
    )
) STRICT;

CREATE TABLE recovery_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    state TEXT NOT NULL CHECK (state IN ('ready', 'recovery-required')),
    detail TEXT,
    serving_mount_revision INTEGER NOT NULL CHECK (serving_mount_revision >= 0),
    failed_mutation_id BLOB CHECK (
        failed_mutation_id IS NULL OR length(failed_mutation_id) = 16
    ),
    updated_at INTEGER NOT NULL
) STRICT;

INSERT INTO recovery_state(
    singleton,
    state,
    detail,
    serving_mount_revision,
    failed_mutation_id,
    updated_at
)
VALUES (1, 'ready', NULL, 0, NULL, unixepoch());

CREATE INDEX mounts_provider_digest_idx ON mounts(provider_digest);
CREATE INDEX mounts_credential_idx ON mounts(
    credential_provider_name,
    credential_scheme,
    credential_account
);
