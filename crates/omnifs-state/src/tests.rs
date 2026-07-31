use super::*;
use crate::paths::{CLONE_CACHE_DIR, PROJECTION_CACHE_DIR, StorePaths, WASMTIME_CACHE_DIR};
use omnifs_api::{
    ActionKind, ActionPhase, AttachmentDefinition, CredentialDefinition, MountResourceDefinition,
    NormalizedResourceSet, ProviderDefinition, ResourceDefinition, ResourceLimits,
};
use omnifs_core::{ActionId, AttachmentSpec, AttachmentVersion, ProviderRef, ResourceName};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

#[test]
fn daemon_log_is_owned_by_private_daemon_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = DaemonStatePaths::new(temp.path().join("daemon-state"));
    drop(open_daemon_log(&paths).unwrap());
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

#[test]
fn attachment_paths_are_private_and_name_scoped() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let name = ResourceName::new("local").unwrap();
    paths.prepare().unwrap();
    paths.prepare_attachment_runtime(&name).unwrap();
    drop(paths.open_attachment_log(&name).unwrap());

    for path in [
        paths.runtime(),
        paths.attachments_runtime(),
        paths.attachment_runtime(&name),
        paths.guest_images_cache(),
        paths.attachment_logs(),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        std::fs::metadata(paths.attachment_log(&name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the complete tombstone lifecycle must remain one restart test"
)]
async fn attachment_instance_tombstone_survives_restart_and_clear() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let name = ResourceName::new("local").unwrap();
    let spec = AttachmentSpec::new(
        if cfg!(target_os = "linux") {
            omnifs_core::fs::Protocol::Fuse
        } else {
            omnifs_core::fs::Protocol::Nfs
        },
        omnifs_core::fs::Runtime::Host,
        PathBuf::from("/tmp/omnifs-attachment-state"),
        None,
        None,
    )
    .unwrap();
    let desired = attachment_resource_set(name.clone(), spec.clone());
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(1),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired: desired.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    let instance = store.attachment_instance(&name).await.unwrap().unwrap();
    let mut observation = AttachmentObservation::from_instance(&instance);
    observation.observed_version = instance.desired_version;
    observation.observed_spec = Some(spec);
    observation.phase = AttachmentPhase::Ready;
    observation.runtime_instance = Some("ab".repeat(16));
    observation.last_error_code = Some("transient".to_owned());
    observation.last_error_detail = Some("retry later".to_owned());
    observation.retry_at = Some(123);
    let stored = store
        .write_attachment_observation(observation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.phase, AttachmentPhase::Ready);
    assert!(stored.updated_at > 0);

    let empty = NormalizedResourceSet::new(Vec::new()).unwrap();
    let head = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(2),
            base_revision: head.revision,
            expected_desired_digest: empty.digest(),
            desired: empty,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    let tombstone = store.attachment_instance(&name).await.unwrap().unwrap();
    store.shutdown().await.unwrap();

    let reopened = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reopened.attachment_instance(&name).await.unwrap(),
        Some(tombstone.clone())
    );
    let replacement =
        attachment_resource_set(name.clone(), tombstone.observed_spec.clone().unwrap());
    let head = reopened.resource_snapshot().await.unwrap();
    reopened
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(3),
            base_revision: head.revision,
            expected_desired_digest: replacement.digest(),
            desired: replacement,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        !reopened
            .clear_attachment_instance_if_deleting(name.clone(), tombstone.runtime_instance.clone())
            .await
            .unwrap()
    );
    let empty = NormalizedResourceSet::new(Vec::new()).unwrap();
    let head = reopened.resource_snapshot().await.unwrap();
    reopened
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(4),
            base_revision: head.revision,
            expected_desired_digest: empty.digest(),
            desired: empty,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        reopened
            .clear_attachment_instance_if_deleting(name.clone(), tombstone.runtime_instance.clone())
            .await
            .unwrap()
    );
    assert_eq!(reopened.attachment_instance(&name).await.unwrap(), None);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_attachment_phase_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    let name = ResourceName::new("local").unwrap();
    let spec = AttachmentSpec::new(
        if cfg!(target_os = "linux") {
            omnifs_core::fs::Protocol::Fuse
        } else {
            omnifs_core::fs::Protocol::Nfs
        },
        omnifs_core::fs::Runtime::Host,
        PathBuf::from("/tmp/omnifs-corrupt-attachment"),
        None,
        None,
    )
    .unwrap();
    let desired = attachment_resource_set(name.clone(), spec);
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(5),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let mut connection = store.reads.acquire().await.unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("UPDATE attachment_instances SET phase = 'unknown' WHERE name = ?1")
        .bind(name.as_str())
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);

    let error = store.attachment_instance(&name).await.unwrap_err();
    assert!(error.to_string().contains("phase `unknown`"));
    store.shutdown().await.unwrap();
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
    let paths = StorePaths::under_root(&temp.path().join("daemon-state"));
    ensure_private_dir(&paths.control_store()).unwrap();
    std::fs::write(paths.database(), b"not sqlite").unwrap();
    ensure_private_dir(&paths.cache()).unwrap();
    std::fs::write(paths.cache().join("keep"), b"cache").unwrap();

    let (store, disposition) =
        StateStore::recreate_control_store(paths.clone(), StateStoreOptions::default())
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
    let paths = StorePaths::under_root(&temp.path().join("daemon-state"));
    ensure_private_dir(paths.root()).unwrap();
    let target = temp.path().join("outside");
    ensure_private_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"outside").unwrap();
    std::os::unix::fs::symlink(&target, paths.control_store()).unwrap();

    let (store, disposition) =
        StateStore::recreate_control_store(paths.clone(), StateStoreOptions::default())
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
    let resources = store.resource_snapshot().await.unwrap();
    assert!(resources.resources.resources().iter().any(
        |resource| matches!(resource, ResourceDefinition::Mount(mount) if mount.name.as_str() == "demo")
    ));

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
    assert!(
        store
            .resource_snapshot()
            .await
            .unwrap()
            .resources
            .resources()
            .is_empty()
    );

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
    store
        .mark_serving(MountRevision::new(created.revision.get().saturating_sub(1)))
        .await
        .unwrap();
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

fn resource_set(provider: ProviderId, mount_config: serde_json::Value) -> NormalizedResourceSet {
    let provider_name = ResourceName::new("demo").unwrap();
    let credential_name = ResourceName::new("alice").unwrap();
    NormalizedResourceSet::new(vec![
        ResourceDefinition::Provider(ProviderDefinition {
            name: provider_name.clone(),
            artifact: provider,
        }),
        ResourceDefinition::Credential(CredentialDefinition {
            name: credential_name.clone(),
            provider: provider_name.clone(),
            scheme: "oauth".to_owned(),
            account: "alice".to_owned(),
        }),
        ResourceDefinition::Mount(MountResourceDefinition {
            name: ResourceName::new("demo-mount").unwrap(),
            provider: provider_name,
            credential: Some(credential_name),
            config: mount_config,
            limits: Some(ResourceLimits {
                max_memory_mb: Some(64),
                max_fetch_blob_bytes: None,
            }),
        }),
        ResourceDefinition::Attachment(AttachmentDefinition {
            name: ResourceName::new("demo-fs").unwrap(),
            spec: AttachmentSpec::new(
                if cfg!(target_os = "linux") {
                    omnifs_core::fs::Protocol::Fuse
                } else {
                    omnifs_core::fs::Protocol::Nfs
                },
                omnifs_core::fs::Runtime::Host,
                PathBuf::from("/tmp/omnifs-resource-test"),
                None,
                None,
            )
            .unwrap(),
        }),
    ])
    .unwrap()
}

fn attachment_resource_set(name: ResourceName, spec: AttachmentSpec) -> NormalizedResourceSet {
    NormalizedResourceSet::new(vec![ResourceDefinition::Attachment(AttachmentDefinition {
        name,
        spec,
    })])
    .unwrap()
}

fn attachment_spec(resources: &NormalizedResourceSet) -> AttachmentSpec {
    resources
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Attachment(definition) => Some(definition.spec.clone()),
            _ => None,
        })
        .unwrap()
}

fn attachment_version(resources: &NormalizedResourceSet) -> AttachmentVersion {
    let definition = resources
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Attachment(definition) => Some(definition),
            _ => None,
        })
        .unwrap();
    crate::resource::codec::encode_attachment(definition)
        .unwrap()
        .1
}

fn replace_attachment_spec(
    resources: &NormalizedResourceSet,
    spec: &AttachmentSpec,
) -> NormalizedResourceSet {
    let mut replaced = resources.resources().to_vec();
    for resource in &mut replaced {
        if let ResourceDefinition::Attachment(definition) = resource {
            definition.spec = spec.clone();
        }
    }
    NormalizedResourceSet::new(replaced).unwrap()
}

fn resource_sidecar(provider: ProviderId, material: &[u8]) -> CredentialSecretSidecar {
    let id = omnifs_auth::CredentialId::new("demo", "oauth", "alice").unwrap();
    CredentialSecretSidecar {
        credential: ResourceName::new("alice").unwrap(),
        document: credential_document(
            &id,
            provider,
            AuthRuntimeFingerprint::from_digest([0x77; 32]),
            material,
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_snapshot_contains_all_kinds_sorted_and_non_secret() {
    let (store, provider) = store_with_imported_provider().await;
    let desired = resource_set(provider.id, serde_json::json!({"b": 2, "a": 1}));
    let initial = store.resource_snapshot().await.unwrap();
    let mutation = mutation_id(80);
    let receipt = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation,
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired: desired.clone(),
            credential_secrets: vec![resource_sidecar(provider.id, b"snapshot-secret")],
        })
        .await
        .unwrap();
    assert!(receipt.changed);

    let snapshot = store.resource_snapshot().await.unwrap();
    assert_eq!(snapshot.revision, initial.revision.next().unwrap());
    assert_eq!(snapshot.desired_digest, desired.digest());
    assert_eq!(snapshot.resources, desired);
    let kinds = snapshot
        .resources
        .resources()
        .iter()
        .map(ResourceDefinition::kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            omnifs_core::ResourceKind::Provider,
            omnifs_core::ResourceKind::Credential,
            omnifs_core::ResourceKind::Mount,
            omnifs_core::ResourceKind::Attachment,
        ]
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("snapshot-secret"));
    let sidecar_debug = format!("{:?}", resource_sidecar(provider.id, b"sidecar-secret"));
    assert!(!sidecar_debug.contains("sidecar-secret"));
    store
        .apply_batch(
            mutation_id(81),
            vec![StateOp::CreateMount(MountDocument {
                name: MountName::new("legacy-only").unwrap(),
                provider,
                credential: None,
                limits: None,
                config: serde_json::json!({}),
            })],
        )
        .await
        .unwrap();
    assert_eq!(
        store.resource_snapshot().await.unwrap().resources,
        desired,
        "legacy batches cannot rewrite desired state after declarative apply"
    );
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one crash-safe desired/observed attachment lifecycle
async fn attachment_desired_updates_and_deletion_preserve_observed_runtime_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let attachment_name = ResourceName::new("demo-fs").unwrap();
    let desired_v1 = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    let first_receipt = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(170),
            base_revision: initial.revision,
            expected_desired_digest: desired_v1.digest(),
            desired: desired_v1.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let restart_id = ActionId::from_bytes([0xaa; 16]);
    let restart = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: restart_id,
            attachment: attachment_name.clone(),
            base_action_generation: 0,
        })
        .await
        .unwrap();
    assert_eq!(restart.phase, ActionPhase::Accepted);
    assert_eq!(restart.action_generation, 1);

    let initial_instance = store
        .attachment_instance(&attachment_name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial_instance.phase, AttachmentPhase::Pending);
    assert_eq!(initial_instance.action_generation, 1);
    assert_eq!(
        initial_instance.desired_spec,
        Some(attachment_spec(&desired_v1))
    );
    assert_eq!(
        initial_instance.desired_version,
        Some(attachment_version(&desired_v1))
    );
    assert_eq!(initial_instance.observed_spec, None);
    assert_eq!(initial_instance.observed_version, None);

    let mut ready_observation = AttachmentObservation::from_instance(&initial_instance);
    ready_observation.observed_spec = initial_instance.desired_spec.clone();
    ready_observation.observed_version = initial_instance.desired_version;
    ready_observation.phase = AttachmentPhase::Ready;
    ready_observation.runtime_instance = Some("cd".repeat(16));
    let ready = store
        .write_attachment_observation(ready_observation)
        .await
        .unwrap()
        .unwrap();

    let changed_spec = AttachmentSpec::new(
        ready.desired_spec.as_ref().unwrap().protocol(),
        ready.desired_spec.as_ref().unwrap().runtime(),
        PathBuf::from("/tmp/omnifs-attachment-state-updated"),
        None,
        None,
    )
    .unwrap();
    let desired_v2 = replace_attachment_spec(&desired_v1, &changed_spec);
    let update_receipt = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(171),
            base_revision: first_receipt.revision,
            expected_desired_digest: desired_v2.digest(),
            desired: desired_v2.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    let updated = store
        .attachment_instance(&attachment_name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.phase, AttachmentPhase::Pending);
    assert_eq!(updated.desired_spec, Some(changed_spec));
    assert_eq!(
        updated.desired_version,
        Some(attachment_version(&desired_v2))
    );
    assert_eq!(updated.observed_spec, ready.observed_spec);
    assert_eq!(updated.observed_version, ready.observed_version);
    assert_eq!(updated.runtime_instance, ready.runtime_instance);
    assert_eq!(updated.action_generation, 1);
    assert!(!updated.deleting);

    let retained_resources = desired_v2
        .resources()
        .iter()
        .filter(|resource| !matches!(resource, ResourceDefinition::Attachment(_)))
        .cloned()
        .collect();
    let desired_deleted = NormalizedResourceSet::new(retained_resources).unwrap();
    let delete_receipt = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(172),
            base_revision: update_receipt.revision,
            expected_desired_digest: desired_deleted.digest(),
            desired: desired_deleted,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    assert!(delete_receipt.deleted >= 1);
    let tombstone = store
        .attachment_instance(&attachment_name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tombstone.phase, AttachmentPhase::Deleting);
    assert_eq!(tombstone.desired_spec, None);
    assert_eq!(tombstone.desired_version, None);
    assert_eq!(tombstone.observed_spec, ready.observed_spec);
    assert_eq!(tombstone.observed_version, ready.observed_version);
    assert_eq!(tombstone.runtime_instance, ready.runtime_instance);
    assert!(tombstone.deleting);
    assert_eq!(tombstone.action_generation, 1);

    store.shutdown().await.unwrap();
    let reopened = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .attachment_instance(&attachment_name)
            .await
            .unwrap(),
        Some(tombstone)
    );
    assert_eq!(
        reopened.action_receipt(restart_id).await.unwrap(),
        Some(restart)
    );
    assert_eq!(reopened.pending_actions().await.unwrap().len(), 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one transaction contract with shared setup and row proof
async fn resource_apply_is_atomic_idempotent_and_stamps_changed_rows() {
    let (store, provider) = store_with_imported_provider().await;
    let desired = resource_set(provider.id, serde_json::json!({"a": 1}));
    let initial = store.resource_snapshot().await.unwrap();
    let first_id = mutation_id(81);
    let first = ResourceApplyRequest {
        mutation_id: first_id,
        base_revision: initial.revision,
        expected_desired_digest: desired.digest(),
        desired: desired.clone(),
        credential_secrets: vec![resource_sidecar(provider.id, b"first-secret")],
    };
    let receipt = store.apply_resources(first).await.unwrap();
    assert_eq!(
        (receipt.created, receipt.updated, receipt.deleted),
        (4, 0, 0)
    );
    assert_eq!(receipt.revision, initial.revision.next().unwrap());

    let retry = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: first_id,
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired: desired.clone(),
            credential_secrets: vec![resource_sidecar(provider.id, b"different-secret")],
        })
        .await
        .unwrap();
    assert_eq!(retry, receipt);
    let stored = store
        .get_credential(&omnifs_auth::CredentialId::new("demo", "oauth", "alice").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.summary.last_mutation_id, first_id);

    let mismatch = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: first_id,
            base_revision: receipt.revision,
            expected_desired_digest: desired.digest(),
            desired: desired.clone(),
            credential_secrets: vec![resource_sidecar(provider.id, b"ignored")],
        })
        .await
        .unwrap_err();
    assert!(matches!(mismatch, ResourceApplyError::MutationIdReuse(_)));

    let unchanged = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(82),
            base_revision: receipt.revision,
            expected_desired_digest: desired.digest(),
            desired: desired.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.revision, receipt.revision);

    let mut changed = desired.clone();
    let mount = changed
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Mount(mount) => Some(mount.clone()),
            _ => None,
        })
        .unwrap();
    let mut resources = changed.resources().to_vec();
    resources.retain(|resource| !matches!(resource, ResourceDefinition::Mount(_)));
    resources.push(ResourceDefinition::Mount(MountResourceDefinition {
        config: serde_json::json!({"a": 2}),
        ..mount
    }));
    changed = NormalizedResourceSet::new(resources).unwrap();

    let stale = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(83),
            base_revision: initial.revision,
            expected_desired_digest: changed.digest(),
            desired: changed.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(stale, ResourceApplyError::StaleRevision { .. }));

    sqlx::query("CREATE TRIGGER fail_mount_resource_update BEFORE UPDATE ON mount_resources BEGIN SELECT RAISE(ABORT, 'test rollback'); END")
        .execute(&store.reads)
        .await
        .unwrap();
    let rollback = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(84),
            base_revision: receipt.revision,
            expected_desired_digest: changed.digest(),
            desired: changed.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(rollback, ResourceApplyError::Store(_)));
    sqlx::query("DROP TRIGGER fail_mount_resource_update")
        .execute(&store.reads)
        .await
        .unwrap();
    let after_rollback = store.resource_snapshot().await.unwrap();
    assert_eq!(after_rollback.resources, desired);
    assert_eq!(after_rollback.revision, receipt.revision);

    let applied = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(85),
            base_revision: receipt.revision,
            expected_desired_digest: changed.digest(),
            desired: changed,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        (applied.created, applied.updated, applied.deleted),
        (0, 1, 0)
    );
    let revision = applied.revision;
    let row: Vec<u8> = sqlx::query_scalar(
        "SELECT last_mutation_id FROM mount_resources WHERE name = 'demo-mount'",
    )
    .fetch_one(&store.reads)
    .await
    .unwrap();
    assert_eq!(row, mutation_id(85).as_bytes());
    let provider_row: Vec<u8> =
        sqlx::query_scalar("SELECT last_mutation_id FROM provider_resources WHERE name = 'demo'")
            .fetch_one(&store.reads)
            .await
            .unwrap();
    assert_eq!(provider_row, first_id.as_bytes());
    let credential_row: Vec<u8> = sqlx::query_scalar(
        "SELECT last_mutation_id FROM credential_resources WHERE name = 'alice'",
    )
    .fetch_one(&store.reads)
    .await
    .unwrap();
    assert_eq!(credential_row, first_id.as_bytes());
    let attachment_row: Vec<u8> = sqlx::query_scalar(
        "SELECT last_mutation_id FROM attachment_resources WHERE name = 'demo-fs'",
    )
    .fetch_one(&store.reads)
    .await
    .unwrap();
    assert_eq!(attachment_row, first_id.as_bytes());
    assert_eq!(store.resource_snapshot().await.unwrap().revision, revision);
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_stored_resource_reports_table_and_name() {
    let (store, provider) = store_with_imported_provider().await;
    let desired = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(86),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: vec![resource_sidecar(provider.id, b"corrupt-test")],
        })
        .await
        .unwrap();
    sqlx::query("UPDATE attachment_resources SET canonical = X'00' WHERE name = 'demo-fs'")
        .execute(&store.reads)
        .await
        .unwrap();
    let error = store.resource_snapshot().await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("decode attachment resource `demo-fs`"));
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one two-open migration scenario with shared legacy state
async fn legacy_resource_backfill_is_deterministic_and_excludes_deleted_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let bytes_one = provider_wasm(8);
    let bytes_two = provider_wasm(9);
    let id_one = ProviderId::from_wasm_bytes(&bytes_one);
    let id_two = ProviderId::from_wasm_bytes(&bytes_two);
    let ref_one = upload_and_import(&store, id_one, &bytes_one)
        .await
        .reference;
    let ref_two = upload_and_import(&store, id_two, &bytes_two)
        .await
        .reference;
    let active = omnifs_auth::CredentialId::new("demo", "oauth", "alice").unwrap();
    let deleted = omnifs_auth::CredentialId::new("demo", "oauth", "gone").unwrap();
    store
        .apply_batch(
            mutation_id(87),
            vec![
                StateOp::SubmitCredential(credential_document(
                    &active,
                    id_two,
                    AuthRuntimeFingerprint::from_digest([0x11; 32]),
                    b"active",
                )),
                StateOp::SubmitCredential(credential_document(
                    &deleted,
                    id_one,
                    AuthRuntimeFingerprint::from_digest([0x12; 32]),
                    b"deleted",
                )),
            ],
        )
        .await
        .unwrap();
    store
        .apply_batch(mutation_id(88), vec![StateOp::DeleteCredential(deleted)])
        .await
        .unwrap();
    store
        .apply_batch(
            mutation_id(89),
            vec![
                StateOp::CreateMount(MountDocument {
                    name: MountName::new("one").unwrap(),
                    provider: ref_one,
                    credential: None,
                    limits: None,
                    config: serde_json::json!({}),
                }),
                StateOp::CreateMount(MountDocument {
                    name: MountName::new("two").unwrap(),
                    provider: ref_two,
                    credential: Some(active),
                    limits: None,
                    config: serde_json::json!({}),
                }),
            ],
        )
        .await
        .unwrap();

    sqlx::query("DELETE FROM attachment_resources")
        .execute(&store.reads)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mount_resources")
        .execute(&store.reads)
        .await
        .unwrap();
    sqlx::query("DELETE FROM credential_resources")
        .execute(&store.reads)
        .await
        .unwrap();
    sqlx::query("DELETE FROM provider_resources")
        .execute(&store.reads)
        .await
        .unwrap();
    sqlx::query("UPDATE resource_state SET initialized = 0")
        .execute(&store.reads)
        .await
        .unwrap();
    store.shutdown().await.unwrap();

    let reopened = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let first = reopened.resource_snapshot().await.unwrap();
    assert_eq!(
        first
            .resources
            .resources()
            .iter()
            .filter(|resource| matches!(resource, ResourceDefinition::Provider(_)))
            .count(),
        2
    );
    assert_eq!(
        first
            .resources
            .resources()
            .iter()
            .filter(|resource| matches!(resource, ResourceDefinition::Credential(_)))
            .count(),
        1
    );
    assert_eq!(
        first
            .resources
            .resources()
            .iter()
            .filter(|resource| matches!(resource, ResourceDefinition::Mount(_)))
            .count(),
        2
    );
    assert!(
        !first
            .resources
            .resources()
            .iter()
            .any(|resource| matches!(resource, ResourceDefinition::Attachment(_)))
    );
    reopened.shutdown().await.unwrap();

    let reopened = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(reopened.resource_snapshot().await.unwrap(), first);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one lost-reply and restart action lifecycle
async fn credential_actions_are_durable_idempotent_and_generation_guarded() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let desired = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(90),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let unavailable = store
        .accept_credential_action(CredentialActionRequest {
            action_id: ActionId::from_bytes([0x90; 16]),
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 0,
            operation: CredentialActionOperation::Revoke,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        unavailable,
        ActionWriteError::ActionUnavailable(name) if name.as_str() == "alice"
    ));
    assert!(store.pending_actions().await.unwrap().is_empty());

    let first_id = ActionId::from_bytes([0x91; 16]);
    let first = store
        .accept_credential_action(CredentialActionRequest {
            action_id: first_id,
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 0,
            operation: CredentialActionOperation::SetMaterial(
                resource_sidecar(provider.id, b"first-action-secret").document,
            ),
        })
        .await
        .unwrap();
    assert_eq!(first.kind, ActionKind::SetCredentialMaterial);
    assert_eq!(first.action_generation, 1);
    assert_eq!(first.phase, ActionPhase::Accepted);
    assert_eq!(store.pending_actions().await.unwrap(), vec![first.clone()]);

    let mut retry_document = resource_sidecar(provider.id, b"different-secret").document;
    retry_document.auth_fingerprint = AuthRuntimeFingerprint::from_digest([0x88; 32]);
    let retry = store
        .accept_credential_action(CredentialActionRequest {
            action_id: first_id,
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 0,
            operation: CredentialActionOperation::SetMaterial(retry_document),
        })
        .await
        .unwrap();
    assert_eq!(retry, first);
    let stored = store
        .get_credential(&omnifs_auth::CredentialId::new("demo", "oauth", "alice").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.material.expose(), b"first-action-secret");

    let reused = store
        .accept_credential_action(CredentialActionRequest {
            action_id: first_id,
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 1,
            operation: CredentialActionOperation::SetMaterial(
                resource_sidecar(provider.id, b"ignored").document,
            ),
        })
        .await
        .unwrap_err();
    assert!(matches!(reused, ActionWriteError::IdReuse(id) if id == first_id));

    let busy_id = ActionId::from_bytes([0x92; 16]);
    let busy = store
        .accept_credential_action(CredentialActionRequest {
            action_id: busy_id,
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 1,
            operation: CredentialActionOperation::Revoke,
        })
        .await
        .unwrap_err();
    assert!(matches!(busy, ActionWriteError::Busy { action_id, .. } if action_id == first_id));
    store.shutdown().await.unwrap();

    let reopened = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reopened.action_receipt(first_id).await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(reopened.pending_actions().await.unwrap(), vec![first]);
    reopened
        .transition_action(first_id, ActionPhase::Ready, None, None)
        .await
        .unwrap();

    let stale = reopened
        .accept_credential_action(CredentialActionRequest {
            action_id: ActionId::from_bytes([0x93; 16]),
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 0,
            operation: CredentialActionOperation::Revoke,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        ActionWriteError::GenerationConflict { actual: 1, .. }
    ));
    let revoke = reopened
        .accept_credential_action(CredentialActionRequest {
            action_id: ActionId::from_bytes([0x94; 16]),
            credential: ResourceName::new("alice").unwrap(),
            expected_generation: 1,
            operation: CredentialActionOperation::Revoke,
        })
        .await
        .unwrap();
    assert_eq!(revoke.kind, ActionKind::RevokeCredential);
    assert_eq!(revoke.action_generation, 2);
    assert_eq!(reopened.pending_actions().await.unwrap(), vec![revoke]);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one restart action acceptance and reopen lifecycle
async fn attachment_restart_actions_are_durable_and_generation_guarded() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StorePaths::under_root(&temp.path().join("state"));
    let store = StateStore::open_paths(paths.clone(), StateStoreOptions::default())
        .await
        .unwrap();
    let missing = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: ActionId::from_bytes([0xa0; 16]),
            attachment: ResourceName::new("missing-fs").unwrap(),
            base_action_generation: 0,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        missing,
        ActionWriteError::AttachmentResourceNotFound(name) if name.as_str() == "missing-fs"
    ));

    let bytes = provider_wasm(8);
    let provider_id = ProviderId::from_wasm_bytes(&bytes);
    let provider = upload_and_import(&store, provider_id, &bytes)
        .await
        .reference;
    let desired = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    let _applied = store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(160),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
    let attachment = ResourceName::new("demo-fs").unwrap();
    let first_id = ActionId::from_bytes([0xa1; 16]);
    let first = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: first_id,
            attachment: attachment.clone(),
            base_action_generation: 0,
        })
        .await
        .unwrap();
    assert_eq!(first.kind, ActionKind::RestartAttachment);
    assert_eq!(first.target.name, attachment);
    assert_eq!(first.action_generation, 1);
    assert_eq!(first.phase, ActionPhase::Accepted);
    let instance = store
        .attachment_instance(&ResourceName::new("demo-fs").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.action_generation, 1);
    assert_eq!(store.pending_actions().await.unwrap(), vec![first.clone()]);

    let replay = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: first_id,
            attachment: ResourceName::new("demo-fs").unwrap(),
            base_action_generation: 0,
        })
        .await
        .unwrap();
    assert_eq!(replay, first);

    let reused = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: first_id,
            attachment: ResourceName::new("demo-fs").unwrap(),
            base_action_generation: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(reused, ActionWriteError::IdReuse(id) if id == first_id));

    let busy_id = ActionId::from_bytes([0xa2; 16]);
    let busy = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: busy_id,
            attachment: ResourceName::new("demo-fs").unwrap(),
            base_action_generation: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        busy,
        ActionWriteError::AttachmentBusy { action_id, .. } if action_id == first_id
    ));
    store.shutdown().await.unwrap();

    let reopened = StateStore::open_paths(paths, StateStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reopened.action_receipt(first_id).await.unwrap(),
        Some(first)
    );
    assert_eq!(reopened.pending_actions().await.unwrap().len(), 1);
    assert_eq!(
        reopened
            .attachment_instance(&ResourceName::new("demo-fs").unwrap())
            .await
            .unwrap()
            .unwrap()
            .action_generation,
        1
    );
    reopened
        .transition_action(first_id, ActionPhase::Ready, None, None)
        .await
        .unwrap();

    let stale = reopened
        .accept_attachment_action(AttachmentActionRequest {
            action_id: ActionId::from_bytes([0xa3; 16]),
            attachment: ResourceName::new("demo-fs").unwrap(),
            base_action_generation: 0,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        ActionWriteError::AttachmentGenerationConflict {
            expected: 0,
            actual: 1,
            ..
        }
    ));
    let second = reopened
        .accept_attachment_action(AttachmentActionRequest {
            action_id: ActionId::from_bytes([0xa4; 16]),
            attachment: ResourceName::new("demo-fs").unwrap(),
            base_action_generation: 1,
        })
        .await
        .unwrap();
    assert_eq!(second.action_generation, 2);
    assert_eq!(reopened.pending_actions().await.unwrap(), vec![second]);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_attachment_observation_cannot_overwrite_new_desired_state() {
    let (store, provider) = store_with_imported_provider().await;
    let desired_v1 = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(230),
            base_revision: initial.revision,
            expected_desired_digest: desired_v1.digest(),
            desired: desired_v1.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let name = ResourceName::new("demo-fs").unwrap();
    let instance = store.attachment_instance(&name).await.unwrap().unwrap();
    let mut starting = AttachmentObservation::from_instance(&instance);
    starting.observed_version = instance.desired_version;
    starting.observed_spec = instance.desired_spec.clone();
    starting.phase = AttachmentPhase::Starting;
    starting.runtime_instance = Some("ef".repeat(16));
    let observed = store
        .write_attachment_observation(starting)
        .await
        .unwrap()
        .unwrap();

    let changed_spec = AttachmentSpec::new(
        observed.desired_spec.as_ref().unwrap().protocol(),
        observed.desired_spec.as_ref().unwrap().runtime(),
        PathBuf::from("/tmp/omnifs-observation-cas-v2"),
        None,
        None,
    )
    .unwrap();
    let desired_v2 = replace_attachment_spec(&desired_v1, &changed_spec);
    let head = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(231),
            base_revision: head.revision,
            expected_desired_digest: desired_v2.digest(),
            desired: desired_v2.clone(),
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let mut stale_ready = AttachmentObservation::from_instance(&observed);
    stale_ready.phase = AttachmentPhase::Ready;
    assert_eq!(
        store
            .write_attachment_observation(stale_ready)
            .await
            .unwrap(),
        None
    );

    let current = store.attachment_instance(&name).await.unwrap().unwrap();
    assert_eq!(current.desired_spec, Some(changed_spec));
    assert_eq!(
        current.desired_version,
        Some(attachment_version(&desired_v2))
    );
    assert_eq!(current.observed_spec, observed.observed_spec);
    assert_eq!(current.observed_version, observed.observed_version);
    assert_eq!(current.runtime_instance, observed.runtime_instance);
    assert_eq!(current.phase, AttachmentPhase::Pending);
    assert_eq!(current.action_generation, 0);
    assert!(!current.deleting);
    store.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_attachment_observation_cannot_lower_restart_generation_or_mark_ready() {
    let (store, provider) = store_with_imported_provider().await;
    let desired = resource_set(provider.id, serde_json::json!({}));
    let initial = store.resource_snapshot().await.unwrap();
    store
        .apply_resources(ResourceApplyRequest {
            mutation_id: mutation_id(232),
            base_revision: initial.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();

    let name = ResourceName::new("demo-fs").unwrap();
    let instance = store.attachment_instance(&name).await.unwrap().unwrap();
    let mut stale_ready = AttachmentObservation::from_instance(&instance);
    stale_ready.observed_version = instance.desired_version;
    stale_ready.observed_spec = instance.desired_spec.clone();
    stale_ready.phase = AttachmentPhase::Ready;
    stale_ready.runtime_instance = Some("fa".repeat(16));

    let receipt = store
        .accept_attachment_action(AttachmentActionRequest {
            action_id: ActionId::from_bytes([0xe8; 16]),
            attachment: name.clone(),
            base_action_generation: 0,
        })
        .await
        .unwrap();
    assert_eq!(receipt.action_generation, 1);
    assert_eq!(
        store
            .write_attachment_observation(stale_ready)
            .await
            .unwrap(),
        None
    );

    let current = store.attachment_instance(&name).await.unwrap().unwrap();
    assert_eq!(current.action_generation, 1);
    assert_eq!(current.phase, AttachmentPhase::Pending);
    assert_eq!(current.observed_version, None);
    assert_eq!(current.observed_spec, None);
    assert_eq!(current.runtime_instance, None);
    store.shutdown().await.unwrap();
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
