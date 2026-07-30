use super::*;
use omnifs_core::ProviderRef;
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn daemon_log_is_owned_by_private_daemon_state() {
    let temp = tempfile::tempdir().unwrap();
    let endpoint = Bootstrap::<Daemon>::under_root(temp.path());
    drop(open_daemon_log(&endpoint).unwrap());
    let path = temp.path().join("daemon-state/logs/daemon.log");
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(temp.path().join("daemon-state/logs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
async fn opens_migrates_and_joins_the_writer() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();

    assert_eq!(store.mount_revision().await.unwrap(), MountRevision::new(0));
    assert_eq!(
        store.serving_state().await.unwrap(),
        ServingState {
            recovery: RecoveryState::Ready,
            revision: MountRevision::default(),
            failed_mutation: None,
        }
    );
    let engine = store.engine_paths();
    assert_eq!(
        engine.projection_cache(),
        paths.cache().join(PROJECTION_CACHE_DIR)
    );
    assert_eq!(
        engine.wasmtime_cache(),
        paths.cache().join(WASMTIME_CACHE_DIR)
    );
    assert_eq!(engine.clone_cache(), paths.cache().join(CLONE_CACHE_DIR));
    store
        .mark_recovery_required(None, "activation failed".to_owned())
        .await
        .unwrap();
    assert_eq!(
        store.serving_state().await.unwrap().recovery,
        RecoveryState::RecoveryRequired {
            detail: "activation failed".to_owned()
        }
    );
    store.shutdown().await.unwrap();

    assert_eq!(
        std::fs::metadata(paths.root())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(paths.database())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn attach_port_is_pinned_once() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let port = NonZeroU16::new(23_456).unwrap();
    assert_eq!(store.attach_port().await.unwrap(), None);
    store.persist_attach_port(port).await.unwrap();
    store.persist_attach_port(port).await.unwrap();
    assert_eq!(store.attach_port().await.unwrap(), Some(port));
    assert!(
        store
            .persist_attach_port(NonZeroU16::new(23_457).unwrap())
            .await
            .is_err()
    );
    assert_eq!(store.attach_port().await.unwrap(), Some(port));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleans_only_stale_staging_files() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    ensure_private_dir(&paths.staging()).unwrap();
    std::fs::write(paths.staging().join("partial"), b"bytes").unwrap();

    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    assert!(std::fs::read_dir(paths.staging()).unwrap().next().is_none());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_corrupt_database() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    ensure_private_dir(&paths.control_store()).unwrap();
    std::fs::write(paths.database(), b"not sqlite").unwrap();

    let error = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .err()
        .expect("corrupt store must fail");
    assert!(error.to_string().contains("StateStore"));
}

#[tokio::test]
async fn recreates_and_archives_a_corrupt_control_store() {
    let temp = tempfile::tempdir().unwrap();
    let endpoint = Bootstrap::<Daemon>::under_root(temp.path());
    let paths = StorePaths::for_endpoint(&endpoint);
    ensure_private_dir(&paths.control_store()).unwrap();
    std::fs::write(paths.database(), b"not sqlite").unwrap();
    ensure_private_dir(&paths.cache()).unwrap();
    std::fs::write(paths.cache().join("keep"), b"cache").unwrap();

    let (store, disposition) =
        StateStore::recreate_control_store(&endpoint, StateStoreOptions::default())
            .await
            .unwrap();

    assert_eq!(
        disposition,
        ControlStoreRepairDisposition::CorruptStoreArchived
    );
    assert_eq!(store.mount_revision().await.unwrap(), MountRevision::new(0));
    assert_eq!(std::fs::read(paths.cache().join("keep")).unwrap(), b"cache");
    let archives = std::fs::read_dir(paths.root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("control-store.corrupt.")
        })
        .count();
    assert_eq!(archives, 1);
    store.shutdown().await.unwrap();
}

#[test]
fn control_store_rollback_restores_the_exact_archived_entry() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    ensure_private_dir(paths.root()).unwrap();
    ensure_private_dir(&paths.control_store()).unwrap();
    std::fs::write(paths.database(), b"original").unwrap();
    let archive = paths.archive_control_store().unwrap().unwrap();
    ensure_private_dir(&paths.control_store()).unwrap();
    std::fs::write(paths.database(), b"replacement").unwrap();

    paths.rollback_control_store(Some(&archive)).unwrap();

    assert_eq!(std::fs::read(paths.database()).unwrap(), b"original");
    assert!(!archive.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn repair_archives_a_symlink_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let endpoint = Bootstrap::<Daemon>::under_root(temp.path());
    let paths = StorePaths::for_endpoint(&endpoint);
    ensure_private_dir(paths.root()).unwrap();
    let target = temp.path().join("outside");
    ensure_private_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"outside").unwrap();
    std::os::unix::fs::symlink(&target, paths.control_store()).unwrap();

    let (store, disposition) =
        StateStore::recreate_control_store(&endpoint, StateStoreOptions::default())
            .await
            .unwrap();

    assert_eq!(
        disposition,
        ControlStoreRepairDisposition::CorruptStoreArchived
    );
    assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"outside");
    assert!(
        !std::fs::symlink_metadata(paths.control_store())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn imports_verifies_and_repairs_provider_rows() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(PROVIDER_CHUNK_BYTES * 2);
    let id = ProviderId::from_wasm_bytes(&bytes);

    let outcome = upload_and_import(&store, id, &bytes).await;
    assert_eq!(outcome.disposition, ProviderImportDisposition::Inserted);
    let stored = store.load_provider(id).await.unwrap().unwrap();
    assert_eq!(stored.bytes, bytes);
    assert_eq!(stored.reference, outcome.reference);
    assert_eq!(stored.manifest.id, "demo");
    assert!(std::fs::read_dir(paths.staging()).unwrap().next().is_none());

    let outcome = upload_and_import(&store, id, &bytes).await;
    assert_eq!(outcome.disposition, ProviderImportDisposition::Unchanged);

    sqlx::query("UPDATE providers SET wasm = zeroblob(wasm_length) WHERE digest = ?1")
        .bind(id.as_bytes().as_slice())
        .execute(&store.reads)
        .await
        .unwrap();
    assert!(store.load_provider(id).await.is_err());
    let outcome = upload_and_import(&store, id, &bytes).await;
    assert_eq!(outcome.disposition, ProviderImportDisposition::Repaired);
    assert_eq!(store.load_provider(id).await.unwrap().unwrap().bytes, bytes);

    let expected_metadata = store
        .load_provider_metadata(id)
        .await
        .unwrap()
        .unwrap()
        .document;
    let mut altered_metadata = expected_metadata.clone();
    altered_metadata.push(b' ');
    sqlx::query(
        "UPDATE providers SET name = 'wrong', version = 'corrupt', metadata = ?2 \
             WHERE digest = ?1",
    )
    .bind(id.as_bytes().as_slice())
    .bind(altered_metadata)
    .execute(&store.reads)
    .await
    .unwrap();
    assert!(store.load_provider(id).await.is_err());
    assert!(store.load_provider_metadata(id).await.is_err());

    let outcome = upload_and_import(&store, id, &bytes).await;
    assert_eq!(outcome.disposition, ProviderImportDisposition::Repaired);
    let stored = store.load_provider(id).await.unwrap().unwrap();
    assert_eq!(stored.reference, outcome.reference);
    assert_eq!(stored.bytes, bytes);
    let metadata = store.load_provider_metadata(id).await.unwrap().unwrap();
    assert_eq!(metadata.reference, outcome.reference);
    assert_eq!(metadata.document, expected_metadata);
    store.shutdown().await.unwrap();
}

/// Shared fixture for the mount-batch tests: an open store with one imported
/// provider, ready to mount.
async fn store_with_imported_provider() -> (StateStore, ProviderRef) {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    (store, provider)
}

#[tokio::test(flavor = "multi_thread")]
async fn mount_crud_applies_batches_and_advances_global_revision() {
    let (store, provider) = store_with_imported_provider().await;
    let document = MountDocument {
        name: MountName::new("demo").unwrap(),
        provider,
        credential: None,
        limits: Some(MountLimits {
            max_memory_mb: Some(64),
            max_fetch_blob_bytes: None,
        }),
        config: serde_json::json!({"b": 2, "a": 1}),
    };

    let create_id = mutation_id(1);
    let results = store
        .apply_batch(create_id, vec![StateOp::CreateMount(document.clone())])
        .await
        .unwrap();
    let created = mount_outcome(&results[0]);
    assert_eq!(created.revision, MountRevision::new(1));
    let version = created.version.unwrap();
    assert_eq!(
        store
            .get_mount(&document.name)
            .await
            .unwrap()
            .unwrap()
            .last_mutation_id,
        create_id
    );

    let mut updated = document.clone();
    updated.config = serde_json::json!({"a": 3});
    let update_id = mutation_id(2);
    let results = store
        .apply_batch(update_id, vec![StateOp::UpdateMount(updated)])
        .await
        .unwrap();
    let update_outcome = mount_outcome(&results[0]);
    assert_eq!(update_outcome.revision, MountRevision::new(2));
    let next_version = update_outcome.version.unwrap();
    assert_ne!(version, next_version);
    assert_eq!(
        store
            .get_mount(&document.name)
            .await
            .unwrap()
            .unwrap()
            .last_mutation_id,
        update_id
    );

    let remove_id = mutation_id(3);
    let results = store
        .apply_batch(remove_id, vec![StateOp::RemoveMount(document.name.clone())])
        .await
        .unwrap();
    let removed = mount_outcome(&results[0]);
    assert_eq!(removed.revision, MountRevision::new(3));
    assert!(store.get_mount(&document.name).await.unwrap().is_none());
    assert!(store.list_mounts().await.unwrap().is_empty());

    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mount_batch_ops_reject_duplicate_create_and_missing_target() {
    let (store, provider) = store_with_imported_provider().await;
    let document = MountDocument {
        name: MountName::new("demo").unwrap(),
        provider,
        credential: None,
        limits: None,
        config: serde_json::json!({}),
    };
    let create_id = mutation_id(1);
    store
        .apply_batch(create_id, vec![StateOp::CreateMount(document.clone())])
        .await
        .unwrap();

    // Create is not an upsert: a second create of the same name fails and
    // leaves the existing row untouched.
    let duplicate_err = store
        .apply_batch(mutation_id(2), vec![StateOp::CreateMount(document.clone())])
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate_err,
        BatchError::Op {
            index: 0,
            error: StateOpError::Mount(MountWriteError::AlreadyExists(_)),
        }
    ));
    assert_eq!(store.list_mounts().await.unwrap().len(), 1);
    assert_eq!(
        store
            .get_mount(&document.name)
            .await
            .unwrap()
            .unwrap()
            .last_mutation_id,
        create_id
    );

    // Updating a name that was never created fails with NotFound.
    let mut missing = document.clone();
    missing.name = MountName::new("missing").unwrap();
    let missing_err = store
        .apply_batch(mutation_id(3), vec![StateOp::UpdateMount(missing)])
        .await
        .unwrap_err();
    assert!(matches!(
        missing_err,
        BatchError::Op {
            index: 0,
            error: StateOpError::Mount(MountWriteError::NotFound(_)),
        }
    ));

    // Removing an absent mount fails with NotFound rather than succeeding
    // vacuously.
    let missing_remove = store
        .apply_batch(
            mutation_id(4),
            vec![StateOp::RemoveMount(MountName::new("missing").unwrap())],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        missing_remove,
        BatchError::Op {
            index: 0,
            error: StateOpError::Mount(MountWriteError::NotFound(_)),
        }
    ));

    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_failure_rolls_back_every_earlier_op_in_the_same_batch() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider_id, &bytes).await;
    let credential_id = omnifs_auth::CredentialId::new("demo", "oauth", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x77; 32]);
    let batch_id = mutation_id(1);

    let error = store
        .apply_batch(
            batch_id,
            vec![
                StateOp::SubmitCredential(credential_document(
                    &credential_id,
                    provider_id,
                    fingerprint,
                    b"secret",
                )),
                StateOp::RemoveMount(MountName::new("missing").unwrap()),
            ],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        BatchError::Op {
            index: 1,
            error: StateOpError::Mount(MountWriteError::NotFound(_)),
        }
    ));

    // The first op's write rolled back along with the rest of the batch: no
    // row exists at all, let alone one stamped with the failed batch's id.
    assert!(
        store
            .get_credential(&credential_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.list_credentials().await.unwrap().is_empty());
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_batch_stamps_every_written_row_and_leaves_others_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x88; 32]);

    // A credential written by an earlier batch must keep that batch's id.
    let earlier_id = mutation_id(1);
    let untouched_credential =
        omnifs_auth::CredentialId::new("demo", "untouched", "default").unwrap();
    store
        .apply_batch(
            earlier_id,
            vec![StateOp::SubmitCredential(credential_document(
                &untouched_credential,
                provider_id,
                fingerprint,
                b"untouched",
            ))],
        )
        .await
        .unwrap();

    let batch_id = mutation_id(2);
    let mount_credential = omnifs_auth::CredentialId::new("demo", "mounted", "default").unwrap();
    let mount = MountDocument {
        name: MountName::new("demo").unwrap(),
        provider,
        credential: None,
        limits: None,
        config: serde_json::json!({}),
    };
    store
        .apply_batch(
            batch_id,
            vec![
                StateOp::CreateMount(mount.clone()),
                StateOp::SubmitCredential(credential_document(
                    &mount_credential,
                    provider_id,
                    fingerprint,
                    b"paired",
                )),
            ],
        )
        .await
        .unwrap();

    let stored_mount = store.get_mount(&mount.name).await.unwrap().unwrap();
    assert_eq!(stored_mount.last_mutation_id, batch_id);
    let stored_credential = store
        .get_credential(&mount_credential)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored_credential.summary.last_mutation_id, batch_id);

    let untouched = store
        .get_credential(&untouched_credential)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(untouched.summary.last_mutation_id, earlier_id);
    store.shutdown().await.unwrap();
}

/// Fixture for the serving-snapshot test: one store carrying five live
/// credentials in every non-deleted state, one deleted (tombstoned)
/// credential, and one mount referencing the active credential.
struct SnapshotFixture {
    store: StateStore,
    mount: MountDocument,
    credential_ids: Vec<omnifs_auth::CredentialId>,
}

async fn build_serving_snapshot_fixture() -> SnapshotFixture {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x66; 32]);
    let credential_cases = [
        ("active", b"active".as_slice()),
        ("pending", b"pending".as_slice()),
        ("blocked", b"blocked".as_slice()),
        ("revocation-pending", b"revocation-pending".as_slice()),
        ("revocation-unknown", b"revocation-unknown".as_slice()),
        ("deleted", b"deleted".as_slice()),
    ];
    let mut credential_ids = Vec::new();
    for (index, (scheme, material)) in credential_cases.iter().enumerate() {
        let id = omnifs_auth::CredentialId::new("demo", *scheme, "default").unwrap();
        store
            .apply_batch(
                mutation_id(u8::try_from(index + 1).unwrap()),
                vec![StateOp::SubmitCredential(credential_document(
                    &id,
                    provider_id,
                    fingerprint,
                    material,
                ))],
            )
            .await
            .unwrap();
        credential_ids.push(id);
    }
    let pending = store
        .refresh_credential(
            credential_document(
                &credential_ids[1],
                provider_id,
                fingerprint,
                b"pending-refreshed",
            ),
            CredentialVersion::initial(),
            CredentialRefreshKind::AuthorityChanged,
        )
        .await
        .unwrap();
    assert_eq!(pending.state, CredentialState::PendingRepublish);
    for (id, status) in [
        (&credential_ids[2], CredentialState::Blocked),
        (&credential_ids[3], CredentialState::RevocationPending),
        (&credential_ids[4], CredentialState::RevocationUnknown),
    ] {
        sqlx::query(
            "UPDATE credentials SET status = ?4 \
                 WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .bind(status.as_str())
        .execute(&store.reads)
        .await
        .unwrap();
    }
    store
        .apply_batch(
            mutation_id(7),
            vec![StateOp::DeleteCredential(credential_ids[5].clone())],
        )
        .await
        .unwrap();

    let mount = MountDocument {
        name: MountName::new("snapshot").unwrap(),
        provider,
        credential: Some(credential_ids[0].clone()),
        limits: None,
        config: serde_json::json!({"snapshot": true}),
    };
    store
        .apply_batch(mutation_id(8), vec![StateOp::CreateMount(mount.clone())])
        .await
        .unwrap();

    SnapshotFixture {
        store,
        mount,
        credential_ids,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serving_snapshot_reads_one_exact_non_deleted_durable_head() {
    let SnapshotFixture {
        store,
        mount,
        credential_ids,
    } = build_serving_snapshot_fixture().await;

    let snapshot = store.serving_snapshot().await.unwrap();
    assert_eq!(snapshot.revision, MountRevision::new(1));
    assert_eq!(snapshot.mounts.len(), 1);
    assert_eq!(snapshot.mounts[0].document, mount);
    assert_eq!(snapshot.mounts[0].revision, snapshot.revision);
    assert_eq!(snapshot.credentials.len(), 5);
    for (id, state, material) in [
        (
            &credential_ids[0],
            CredentialState::Active,
            b"active".as_slice(),
        ),
        (
            &credential_ids[1],
            CredentialState::PendingRepublish,
            b"pending-refreshed".as_slice(),
        ),
        (
            &credential_ids[2],
            CredentialState::Blocked,
            b"blocked".as_slice(),
        ),
        (
            &credential_ids[3],
            CredentialState::RevocationPending,
            b"revocation-pending".as_slice(),
        ),
        (
            &credential_ids[4],
            CredentialState::RevocationUnknown,
            b"revocation-unknown".as_slice(),
        ),
    ] {
        let credential = snapshot
            .credentials
            .iter()
            .find(|credential| &credential.summary.id == id)
            .unwrap();
        assert_eq!(credential.summary.state, state);
        assert_eq!(credential.material.expose(), material);
    }
    assert!(
        snapshot
            .credentials
            .iter()
            .all(|credential| credential.summary.id != credential_ids[5])
    );
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_serving_and_recovery_required_round_trip_plain_mutation_ids() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let create_id = mutation_id(1);
    let results = store
        .apply_batch(
            create_id,
            vec![StateOp::CreateMount(MountDocument {
                name: MountName::new("demo").unwrap(),
                provider,
                credential: None,
                limits: None,
                config: serde_json::json!({}),
            })],
        )
        .await
        .unwrap();
    let created = mount_outcome(&results[0]);

    store.mark_serving(created.revision).await.unwrap();
    assert_eq!(
        store.serving_state().await.unwrap(),
        ServingState {
            recovery: RecoveryState::Ready,
            revision: MountRevision::new(1),
            failed_mutation: None,
        }
    );

    let failed_id = mutation_id(2);
    store
        .mark_recovery_required(Some(failed_id), "drain stuck".to_owned())
        .await
        .unwrap();
    assert_eq!(
        store.serving_state().await.unwrap(),
        ServingState {
            recovery: RecoveryState::RecoveryRequired {
                detail: "drain stuck".to_owned(),
            },
            revision: MountRevision::new(1),
            failed_mutation: Some(failed_id),
        }
    );

    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_crud_versions_material_and_retains_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider, &bytes).await;
    let id = omnifs_auth::CredentialId::new("demo", "pat", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x33; 32]);
    let submit_id = mutation_id(1);
    let results = store
        .apply_batch(
            submit_id,
            vec![StateOp::SubmitCredential(static_credential(
                &id,
                provider,
                fingerprint,
                b"first-secret",
            ))],
        )
        .await
        .unwrap();
    let created = credential_outcome(&results[0]);
    assert_eq!(created.version, CredentialVersion::initial());
    assert_eq!(created.generation, CredentialGeneration::initial());
    assert_eq!(created.last_mutation_id, submit_id);
    let loaded = store.get_credential(&id).await.unwrap().unwrap();
    assert_eq!(loaded.material.expose(), b"first-secret");

    // Submit is an unconditional upsert: submitting again bumps version and
    // generation without any compare-and-swap input.
    let replace_id = mutation_id(2);
    let results = store
        .apply_batch(
            replace_id,
            vec![StateOp::SubmitCredential(static_credential(
                &id,
                provider,
                fingerprint,
                b"second-secret",
            ))],
        )
        .await
        .unwrap();
    let replaced = credential_outcome(&results[0]);
    assert_eq!(replaced.version.get(), 2);
    assert_eq!(replaced.generation.get(), 2);
    assert_eq!(replaced.last_mutation_id, replace_id);

    let delete_id = mutation_id(3);
    let results = store
        .apply_batch(delete_id, vec![StateOp::DeleteCredential(id.clone())])
        .await
        .unwrap();
    let deleted = credential_outcome(&results[0]);
    assert_eq!(deleted.state, CredentialState::Deleted);
    assert_eq!(deleted.last_mutation_id, delete_id);
    let tombstone = store.get_credential(&id).await.unwrap().unwrap();
    assert_eq!(tombstone.summary.state, CredentialState::Deleted);
    assert!(tombstone.material.expose().is_empty());
    assert_eq!(store.list_credentials().await.unwrap().len(), 1);

    // Deleting a name that was never submitted fails with NotFound.
    let missing = omnifs_auth::CredentialId::new("demo", "missing", "default").unwrap();
    let missing_err = store
        .apply_batch(mutation_id(4), vec![StateOp::DeleteCredential(missing)])
        .await
        .unwrap_err();
    assert!(matches!(
        missing_err,
        BatchError::Op {
            index: 0,
            error: StateOpError::Credential(CredentialWriteError::NotFound(_)),
        }
    ));
    store.shutdown().await.unwrap();
}

/// Open a store with one submitted OAuth credential, ready for the
/// revocation tests below.
async fn store_with_submitted_oauth_credential() -> (StateStore, omnifs_auth::CredentialId) {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::open_paths(
        StorePaths::under_root(&temp.path().join("state")),
        StateStoreOptions::default(),
    )
    .await
    .unwrap();
    let bytes = provider_wasm(8);
    let provider = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider, &bytes).await;
    let id = omnifs_auth::CredentialId::new("demo", "oauth", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x44; 32]);
    let submitted_results = store
        .apply_batch(
            mutation_id(1),
            vec![StateOp::SubmitCredential(credential_document(
                &id,
                provider,
                fingerprint,
                b"secret payload",
            ))],
        )
        .await
        .unwrap();
    assert_eq!(
        credential_outcome(&submitted_results[0]).scopes,
        vec!["repo"]
    );
    (store, id)
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_revocation_begins_pending_and_resolves_to_unknown_outcome() {
    let (store, id) = store_with_submitted_oauth_credential().await;

    let revoke_id = mutation_id(2);
    let results = store
        .apply_batch(
            revoke_id,
            vec![StateOp::RevokeCredential {
                id: id.clone(),
                scopes: vec!["repo".to_owned()],
            }],
        )
        .await
        .unwrap();
    let pending = credential_outcome(&results[0]);
    assert_eq!(pending.state, CredentialState::RevocationPending);
    assert_eq!(pending.version.get(), 2);
    assert_eq!(pending.generation.get(), 2);
    assert_eq!(pending.scopes, vec!["repo"]);
    assert_eq!(pending.last_mutation_id, revoke_id);
    let resumable = store.pending_credential_revocations().await.unwrap();
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].mutation, revoke_id);
    assert_eq!(resumable[0].credential.material.expose(), b"secret payload");

    let unknown = store
        .finish_credential_revocation(
            id.clone(),
            revoke_id,
            CredentialRevocationFinish::Unknown,
            vec!["repo".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(unknown.state, CredentialState::RevocationUnknown);
    assert_eq!(unknown.scopes, vec!["repo"]);
    assert_eq!(unknown.version.get(), 3);
    assert!(
        store
            .pending_credential_revocations()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose(),
        b"secret payload"
    );
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_revocation_unknown_outcome_can_still_resolve_to_deleted() {
    let (store, id) = store_with_submitted_oauth_credential().await;
    let revoke_id = mutation_id(2);
    store
        .apply_batch(
            revoke_id,
            vec![StateOp::RevokeCredential {
                id: id.clone(),
                scopes: vec!["repo".to_owned()],
            }],
        )
        .await
        .unwrap();
    store
        .finish_credential_revocation(
            id.clone(),
            revoke_id,
            CredentialRevocationFinish::Unknown,
            vec!["repo".to_owned()],
        )
        .await
        .unwrap();

    let deleted = store
        .finish_credential_revocation(
            id.clone(),
            revoke_id,
            CredentialRevocationFinish::Deleted,
            vec!["repo".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(deleted.state, CredentialState::Deleted);
    assert!(deleted.scopes.is_empty());
    assert_eq!(deleted.version.get(), 4);
    assert!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose()
            .is_empty()
    );
    assert!(
        store
            .pending_credential_revocations()
            .await
            .unwrap()
            .is_empty()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_refresh_routine_then_authority_changed_moves_to_pending_republish() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider, &bytes).await;
    let id = omnifs_auth::CredentialId::new("demo", "oauth", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x44; 32]);
    store
        .apply_batch(
            mutation_id(1),
            vec![StateOp::SubmitCredential(credential_document(
                &id,
                provider,
                fingerprint,
                b"initial",
            ))],
        )
        .await
        .unwrap();

    let mut wakeup = store.subscribe_credential_refreshes();
    let routine = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"routine"),
            CredentialVersion::initial(),
            CredentialRefreshKind::Routine,
        )
        .await
        .unwrap();
    assert_eq!(routine.version.get(), 2);
    assert_eq!(routine.generation, CredentialGeneration::initial());
    assert_eq!(routine.state, CredentialState::Active);
    assert!(!wakeup.has_changed().unwrap());
    assert_eq!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose(),
        b"routine"
    );

    let pending = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"authority"),
            routine.version,
            CredentialRefreshKind::AuthorityChanged,
        )
        .await
        .unwrap();
    assert_eq!(pending.version.get(), 3);
    assert_eq!(pending.generation.get(), 2);
    assert_eq!(pending.state, CredentialState::PendingRepublish);
    assert!(wakeup.has_changed().unwrap());
    wakeup.borrow_and_update();
    assert_eq!(
        store.list_credentials().await.unwrap()[0].state,
        CredentialState::PendingRepublish
    );
    assert_eq!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose(),
        b"authority"
    );
    store.shutdown().await.unwrap();
}

/// Open a store with one OAuth credential already sitting in
/// `PendingRepublish`, ready for the reject/activate tests below. Returns
/// the pending refresh outcome so callers have its version and generation.
async fn store_with_credential_pending_republish() -> (
    StateStore,
    omnifs_auth::CredentialId,
    ProviderId,
    AuthRuntimeFingerprint,
    CredentialRefreshOutcome,
) {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider, &bytes).await;
    let id = omnifs_auth::CredentialId::new("demo", "oauth", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x44; 32]);
    store
        .apply_batch(
            mutation_id(1),
            vec![StateOp::SubmitCredential(credential_document(
                &id,
                provider,
                fingerprint,
                b"initial",
            ))],
        )
        .await
        .unwrap();
    let routine = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"routine"),
            CredentialVersion::initial(),
            CredentialRefreshKind::Routine,
        )
        .await
        .unwrap();
    let pending = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"authority"),
            routine.version,
            CredentialRefreshKind::AuthorityChanged,
        )
        .await
        .unwrap();
    (store, id, provider, fingerprint, pending)
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_refresh_rejects_while_pending_but_activation_succeeds() {
    let (store, id, provider, fingerprint, pending) =
        store_with_credential_pending_republish().await;
    let wakeup = store.subscribe_credential_refreshes();

    let repeated = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"authority-again"),
            pending.version,
            CredentialRefreshKind::AuthorityChanged,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        repeated,
        CredentialWriteError::InvalidState {
            expected: "active",
            actual: CredentialState::PendingRepublish,
            ..
        }
    ));
    let unchanged = store.get_credential(&id).await.unwrap().unwrap();
    assert_eq!(unchanged.summary.version, pending.version);
    assert_eq!(unchanged.summary.generation, pending.generation);
    assert_eq!(unchanged.summary.state, CredentialState::PendingRepublish);
    assert_eq!(unchanged.material.expose(), b"authority");

    let active = store
        .activate_refreshed_credential(id.clone(), pending.version, pending.generation)
        .await
        .unwrap();
    assert_eq!(active.version, pending.version);
    assert_eq!(active.generation, pending.generation);
    assert_eq!(active.state, CredentialState::Active);
    assert!(!wakeup.has_changed().unwrap());
    store.shutdown().await.unwrap();
}

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn credential_refresh_rejects_stale_facts_and_states() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider = ProviderId::from_wasm_bytes(&bytes);
    upload_and_import(&store, provider, &bytes).await;
    let id = omnifs_auth::CredentialId::new("demo", "oauth", "default").unwrap();
    let fingerprint = AuthRuntimeFingerprint::from_digest([0x55; 32]);
    let created_results = store
        .apply_batch(
            mutation_id(1),
            vec![StateOp::SubmitCredential(credential_document(
                &id,
                provider,
                fingerprint,
                b"initial",
            ))],
        )
        .await
        .unwrap();
    let created = credential_outcome(&created_results[0]);
    let mismatch = store
        .refresh_credential(
            credential_document(
                &id,
                provider,
                AuthRuntimeFingerprint::from_digest([0x56; 32]),
                b"wrong-facts",
            ),
            created.version,
            CredentialRefreshKind::Routine,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        mismatch,
        CredentialWriteError::FactsMismatch { .. }
    ));
    assert_eq!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose(),
        b"initial"
    );

    let replacement_results = store
        .apply_batch(
            mutation_id(2),
            vec![StateOp::SubmitCredential(credential_document(
                &id,
                provider,
                fingerprint,
                b"replacement",
            ))],
        )
        .await
        .unwrap();
    let replacement = credential_outcome(&replacement_results[0]);
    let stale = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"stale"),
            created.version,
            CredentialRefreshKind::Routine,
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, CredentialWriteError::Conflict { .. }));
    assert_eq!(
        store
            .get_credential(&id)
            .await
            .unwrap()
            .unwrap()
            .material
            .expose(),
        b"replacement"
    );

    let deleted_results = store
        .apply_batch(mutation_id(3), vec![StateOp::DeleteCredential(id.clone())])
        .await
        .unwrap();
    let deleted = credential_outcome(&deleted_results[0]);
    let stale_after_delete = store
        .refresh_credential(
            credential_document(&id, provider, fingerprint, b"overwritten"),
            replacement.version,
            CredentialRefreshKind::Routine,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        stale_after_delete,
        CredentialWriteError::Conflict { .. }
    ));
    let tombstone = store.get_credential(&id).await.unwrap().unwrap();
    assert_eq!(tombstone.summary.version, deleted.version);
    assert_eq!(tombstone.summary.state, CredentialState::Deleted);
    assert!(tombstone.material.expose().is_empty());

    let wrong_state = store
        .activate_refreshed_credential(id.clone(), deleted.version, deleted.generation)
        .await
        .unwrap_err();
    assert!(matches!(
        wrong_state,
        CredentialWriteError::InvalidState { .. }
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_truncated_and_wrong_digest_uploads_without_staging_leaks() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(32);
    let id = ProviderId::from_wasm_bytes(&bytes);

    let mut truncated = store
        .begin_provider_upload("demo.wasm", id, u64::try_from(bytes.len()).unwrap())
        .await
        .unwrap();
    truncated
        .write_chunk(&bytes[..bytes.len() - 1])
        .await
        .unwrap();
    assert!(truncated.finish().await.is_err());
    assert!(std::fs::read_dir(paths.staging()).unwrap().next().is_none());

    let wrong_id = ProviderId::from_wasm_bytes(b"wrong");
    let mut wrong = store
        .begin_provider_upload("demo.wasm", wrong_id, u64::try_from(bytes.len()).unwrap())
        .await
        .unwrap();
    wrong.write_chunk(&bytes).await.unwrap();
    assert!(wrong.finish().await.is_err());
    assert!(std::fs::read_dir(paths.staging()).unwrap().next().is_none());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn enforces_provider_size_and_disk_budget_before_staging() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let options = StateStoreOptions {
        disk_budget_bytes: 1024,
        ..StateStoreOptions::default()
    };
    let store = StateStore::open_paths(paths.clone(), options)
        .await
        .unwrap();
    let id = ProviderId::from_wasm_bytes(b"bytes");

    assert!(
        store
            .begin_provider_upload("demo.wasm", id, MAX_PROVIDER_BYTES + 1)
            .await
            .is_err()
    );
    assert!(
        store
            .begin_provider_upload("demo.wasm", id, 1024)
            .await
            .is_err()
    );
    assert!(std::fs::read_dir(paths.staging()).unwrap().next().is_none());
    store.shutdown().await.unwrap();
}

async fn upload_and_import(
    store: &StateStore,
    id: ProviderId,
    bytes: &[u8],
) -> ProviderImportOutcome {
    let mut upload = store
        .begin_provider_upload("demo.wasm", id, u64::try_from(bytes.len()).unwrap())
        .await
        .unwrap();
    for chunk in bytes.chunks(PROVIDER_CHUNK_BYTES) {
        upload.write_chunk(chunk).await.unwrap();
    }
    store
        .import_provider(upload.finish().await.unwrap())
        .await
        .unwrap()
}

fn mutation_id(byte: u8) -> MutationId {
    MutationId::from_bytes([byte; 16])
}

fn mount_outcome(outcome: &OpOutcome) -> MountMutationOutcome {
    match outcome {
        OpOutcome::Mount(outcome) => outcome.clone(),
        OpOutcome::Credential(_) => panic!("expected a mount op outcome"),
    }
}

fn credential_outcome(outcome: &OpOutcome) -> CredentialMutationOutcome {
    match outcome {
        OpOutcome::Credential(outcome) => outcome.clone(),
        OpOutcome::Mount(_) => panic!("expected a credential op outcome"),
    }
}

fn static_credential(
    id: &omnifs_auth::CredentialId,
    provider: ProviderId,
    auth_fingerprint: AuthRuntimeFingerprint,
    material: &[u8],
) -> CredentialDocument {
    CredentialDocument {
        id: id.clone(),
        provider,
        kind: omnifs_auth::AuthKind::StaticToken,
        auth_fingerprint,
        scopes: Vec::new(),
        material: SecretMaterial::new(material.to_vec()),
    }
}

fn credential_document(
    id: &omnifs_auth::CredentialId,
    provider: ProviderId,
    auth_fingerprint: AuthRuntimeFingerprint,
    material: &[u8],
) -> CredentialDocument {
    CredentialDocument {
        id: id.clone(),
        provider,
        kind: omnifs_auth::AuthKind::OAuth,
        auth_fingerprint,
        scopes: vec!["repo".to_owned()],
        material: SecretMaterial::new(material.to_vec()),
    }
}

fn provider_wasm(description_bytes: usize) -> Vec<u8> {
    let metadata = serde_json::to_vec(&serde_json::json!({
        "id": "demo",
        "displayName": "Demo",
        "description": "x".repeat(description_bytes),
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "refreshIntervalSecs": 0
    }))
    .unwrap();
    let name = omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes();
    let mut payload = Vec::new();
    append_uleb(&mut payload, name.len());
    payload.extend_from_slice(name);
    payload.extend_from_slice(&metadata);

    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    wasm.push(0);
    append_uleb(&mut wasm, payload.len());
    wasm.extend_from_slice(&payload);
    wasm
}

fn append_uleb(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            output.push(byte);
            break;
        }
        output.push(byte | 0x80);
    }
}
