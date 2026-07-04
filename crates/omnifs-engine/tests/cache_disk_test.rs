use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::view::Cache;
use omnifs_engine::test_support::cache::{BatchRecord, Key, Record, RecordKind};

fn p(path: &str) -> Path {
    Path::parse(path).unwrap()
}

#[test]
fn disk_put_batch() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("view")).unwrap();

    let records = vec![
        BatchRecord::new(
            p("/a/title"),
            RecordKind::File,
            None,
            Record::new(RecordKind::File, b"hello\n".to_vec()),
        ),
        BatchRecord::new(
            p("/a/body"),
            RecordKind::File,
            None,
            Record::new(RecordKind::File, b"world\n".to_vec()),
        ),
        BatchRecord::new(
            p("/a"),
            RecordKind::Attr,
            None,
            Record::new(RecordKind::Attr, vec![0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ),
    ];
    cache.put_batch(&records);

    assert!(
        cache
            .get(&Key::new(&p("/a/title"), RecordKind::File))
            .is_some()
    );
    assert!(
        cache
            .get(&Key::new(&p("/a/body"), RecordKind::File))
            .is_some()
    );
    assert!(cache.get(&Key::new(&p("/a"), RecordKind::Attr)).is_some());
}

#[test]
fn disk_invalidate_mount_scoped_prefix_respects_segment_boundaries() {
    // Like disk_invalidate_prefix_respects_segment_boundaries but with
    // mount-scoped typed paths.
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("view")).unwrap();

    for path in [
        "/test/owner/repo",
        "/test/owner/repo/issues",
        "/test/owner/repobaz",
    ] {
        cache.put(
            &Key::new(&p(path), RecordKind::Attr),
            &Record::new(RecordKind::Attr, vec![1]),
        );
    }

    cache.invalidate_prefix(&p("/test/owner/repo"));

    assert!(
        cache
            .get(&Key::new(&p("/test/owner/repo"), RecordKind::Attr))
            .is_none(),
        "/test/owner/repo should be gone"
    );
    assert!(
        cache
            .get(&Key::new(&p("/test/owner/repo/issues"), RecordKind::Attr))
            .is_none(),
        "/test/owner/repo/issues should be gone"
    );
    assert!(
        cache
            .get(&Key::new(&p("/test/owner/repobaz"), RecordKind::Attr))
            .is_some(),
        "/test/owner/repobaz should remain"
    );
}

#[test]
fn disk_invalidate_prefix_respects_segment_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("view")).unwrap();

    for path in ["/owner/repo", "/owner/repo/issues", "/owner/repobaz"] {
        cache.put(
            &Key::new(&p(path), RecordKind::Attr),
            &Record::new(RecordKind::Attr, vec![1]),
        );
    }

    cache.invalidate_prefix(&p("/owner/repo"));

    assert!(
        cache
            .get(&Key::new(&p("/owner/repo"), RecordKind::Attr))
            .is_none()
    );
    assert!(
        cache
            .get(&Key::new(&p("/owner/repo/issues"), RecordKind::Attr))
            .is_none()
    );
    assert!(
        cache
            .get(&Key::new(&p("/owner/repobaz"), RecordKind::Attr))
            .is_some()
    );
}

#[test]
fn disk_keying_distinguishes_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("view")).unwrap();

    let shared_path = p("/owner/repo/README.md");
    let lookup = Record::new(RecordKind::Lookup, b"lookup".to_vec());
    let attr = Record::new(RecordKind::Attr, b"attr".to_vec());

    cache.put(&Key::new(&shared_path, RecordKind::Lookup), &lookup);
    cache.put(&Key::new(&shared_path, RecordKind::Attr), &attr);

    assert!(
        cache
            .get(&Key::new(&shared_path, RecordKind::Lookup))
            .is_some()
    );
    assert!(
        cache
            .get(&Key::new(&shared_path, RecordKind::Attr))
            .is_some()
    );
    assert!(
        cache
            .get(&Key::new(&shared_path, RecordKind::Dirents))
            .is_none()
    );
}

#[test]
fn disk_keying_distinguishes_aux_values() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("view")).unwrap();

    let path = p("/owner/repo/state.txt");
    let v1 = Record::new(RecordKind::File, b"v1".to_vec());
    let v2 = Record::new(RecordKind::File, b"v2".to_vec());

    cache.put(
        &Key::with_aux(&path, RecordKind::File, Some("version:1")),
        &v1,
    );
    cache.put(
        &Key::with_aux(&path, RecordKind::File, Some("version:2")),
        &v2,
    );

    assert_eq!(
        cache
            .get(&Key::with_aux(&path, RecordKind::File, Some("version:1")))
            .unwrap()
            .payload,
        b"v1"
    );
    assert_eq!(
        cache
            .get(&Key::with_aux(&path, RecordKind::File, Some("version:2")))
            .unwrap()
            .payload,
        b"v2"
    );

    cache.delete_exact(&path);
    assert!(
        cache
            .get(&Key::with_aux(&path, RecordKind::File, Some("version:1")))
            .is_none()
    );
    assert!(
        cache
            .get(&Key::with_aux(&path, RecordKind::File, Some("version:2")))
            .is_none()
    );
}
