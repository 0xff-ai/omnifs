CREATE TABLE resource_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    desired_digest BLOB NOT NULL CHECK (length(desired_digest) = 32),
    initialized INTEGER NOT NULL CHECK (initialized IN (0, 1)),
    updated_at INTEGER NOT NULL
) STRICT;

-- BLAKE3("omnifs-resource-set-v1\0" || 0_u64.to_be_bytes()).
INSERT INTO resource_state(
    singleton,
    revision,
    desired_digest,
    initialized,
    updated_at
)
VALUES (
    1,
    0,
    X'adc28defe5460afa3015496b2cd982a5f018e9b66f3b0aca5294a2a0936dafdd',
    0,
    unixepoch()
);

CREATE TABLE provider_resources (
    name TEXT PRIMARY KEY CHECK (
        length(name) BETWEEN 1 AND 32
        AND substr(name, 1, 1) GLOB '[a-z0-9]'
        AND name NOT GLOB '*[^a-z0-9-]*'
    ),
    provider_digest BLOB NOT NULL CHECK (length(provider_digest) = 32),
    revision INTEGER NOT NULL CHECK (revision > 0),
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (provider_digest) REFERENCES providers(digest)
) STRICT;

CREATE TABLE credential_resources (
    name TEXT PRIMARY KEY CHECK (
        length(name) BETWEEN 1 AND 32
        AND substr(name, 1, 1) GLOB '[a-z0-9]'
        AND name NOT GLOB '*[^a-z0-9-]*'
    ),
    provider_name TEXT NOT NULL,
    scheme TEXT NOT NULL CHECK (length(scheme) BETWEEN 1 AND 128),
    account TEXT NOT NULL CHECK (length(account) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision > 0),
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (provider_name) REFERENCES provider_resources(name)
) STRICT;

CREATE TABLE mount_resources (
    name TEXT PRIMARY KEY CHECK (
        length(name) BETWEEN 1 AND 32
        AND substr(name, 1, 1) GLOB '[a-z0-9]'
        AND name NOT GLOB '*[^a-z0-9-]*'
    ),
    canonical BLOB NOT NULL,
    version BLOB NOT NULL CHECK (length(version) = 32),
    provider_name TEXT NOT NULL,
    credential_name TEXT,
    revision INTEGER NOT NULL CHECK (revision > 0),
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (provider_name) REFERENCES provider_resources(name),
    FOREIGN KEY (credential_name) REFERENCES credential_resources(name)
) STRICT;

CREATE TABLE attachment_resources (
    name TEXT PRIMARY KEY CHECK (
        length(name) BETWEEN 1 AND 32
        AND substr(name, 1, 1) GLOB '[a-z0-9]'
        AND name NOT GLOB '*[^a-z0-9-]*'
    ),
    canonical BLOB NOT NULL,
    version BLOB NOT NULL CHECK (length(version) = 32),
    revision INTEGER NOT NULL CHECK (revision > 0),
    last_mutation_id BLOB NOT NULL CHECK (length(last_mutation_id) = 16),
    updated_at INTEGER NOT NULL
) STRICT;

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

CREATE INDEX provider_resources_digest_idx
    ON provider_resources(provider_digest);
CREATE INDEX credential_resources_provider_idx
    ON credential_resources(provider_name);
CREATE INDEX mount_resources_provider_idx
    ON mount_resources(provider_name);
CREATE INDEX mount_resources_credential_idx
    ON mount_resources(credential_name);
CREATE INDEX apply_receipts_created_at_idx
    ON apply_receipts(created_at);
