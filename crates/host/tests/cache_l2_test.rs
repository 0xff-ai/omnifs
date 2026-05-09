use omnifs_host::cache::l2::Cache;
use omnifs_host::cache::{
    AttrPayload, CacheRecord, EntryKindCache, Key, LookupPayload, RecordKind,
};

#[test]
fn l2_put_get_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("browse.redb");
    let l2 = Cache::open(&db_path).unwrap();

    let record = CacheRecord::new(RecordKind::Attr, vec![1, 0, 0, 0, 0, 0, 0, 0, 42]);
    l2.put(
        &Key::new("owner/repo/_issues/_open/1/title", RecordKind::Attr),
        &record,
    )
    .unwrap();

    let got = l2
        .get(&Key::new(
            "owner/repo/_issues/_open/1/title",
            RecordKind::Attr,
        ))
        .unwrap();
    assert!(got.is_some());
    let got = got.unwrap();
    assert_eq!(got.kind, RecordKind::Attr);
    assert_eq!(got.payload, vec![1, 0, 0, 0, 0, 0, 0, 0, 42]);
}

#[test]
fn l2_get_miss() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();
    assert!(
        l2.get(&Key::new("nonexistent", RecordKind::Lookup))
            .unwrap()
            .is_none()
    );
}

#[test]
fn l2_file_small_goes_to_content_table() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();

    let small = vec![0u8; 1024]; // 1 KiB, below 64 KiB threshold
    let record = CacheRecord::new(RecordKind::File, small.clone());
    l2.put(&Key::new("path/to/title", RecordKind::File), &record)
        .unwrap();

    let got = l2
        .get(&Key::new("path/to/title", RecordKind::File))
        .unwrap()
        .unwrap();
    assert_eq!(got.payload, small);
}

#[test]
fn l2_file_large_goes_to_bulk_table() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();

    let large = vec![0u8; 100_000]; // 100 KiB, above 64 KiB threshold
    let record = CacheRecord::new(RecordKind::File, large.clone());
    l2.put(&Key::new("path/to/log", RecordKind::File), &record)
        .unwrap();

    let got = l2
        .get(&Key::new("path/to/log", RecordKind::File))
        .unwrap()
        .unwrap();
    assert_eq!(got.payload, large);
}

#[test]
fn l2_put_batch() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();

    let records = vec![
        (
            "a/title".to_string(),
            RecordKind::File,
            CacheRecord::new(RecordKind::File, b"hello\n".to_vec()),
        ),
        (
            "a/body".to_string(),
            RecordKind::File,
            CacheRecord::new(RecordKind::File, b"world\n".to_vec()),
        ),
        (
            "a".to_string(),
            RecordKind::Attr,
            CacheRecord::new(RecordKind::Attr, vec![0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ),
    ];
    l2.put_batch(&records).unwrap();

    assert!(
        l2.get(&Key::new("a/title", RecordKind::File))
            .unwrap()
            .is_some()
    );
    assert!(
        l2.get(&Key::new("a/body", RecordKind::File))
            .unwrap()
            .is_some()
    );
    assert!(l2.get(&Key::new("a", RecordKind::Attr)).unwrap().is_some());
}

#[test]
fn l2_delete_prefix_respects_segment_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();

    for path in ["owner/repo", "owner/repo/issues", "owner/repobaz"] {
        l2.put(
            &Key::new(path, RecordKind::Attr),
            &CacheRecord::new(RecordKind::Attr, vec![1]),
        )
        .unwrap();
    }

    l2.delete_prefix("owner/repo").unwrap();

    assert!(
        l2.get(&Key::new("owner/repo", RecordKind::Attr))
            .unwrap()
            .is_none()
    );
    assert!(
        l2.get(&Key::new("owner/repo/issues", RecordKind::Attr))
            .unwrap()
            .is_none()
    );
    assert!(
        l2.get(&Key::new("owner/repobaz", RecordKind::Attr))
            .unwrap()
            .is_some()
    );
}

#[test]
fn l2_keying_distinguishes_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let l2 = Cache::open(&dir.path().join("browse.redb")).unwrap();

    let shared_path = "owner/repo/README.md";
    let lookup = CacheRecord::new(
        RecordKind::Lookup,
        LookupPayload::Positive {
            kind: EntryKindCache::File,
            size: 42,
        }
        .serialize()
        .unwrap(),
    );
    let attr = CacheRecord::new(
        RecordKind::Attr,
        AttrPayload {
            kind: EntryKindCache::File,
            size: 42,
        }
        .serialize()
        .unwrap(),
    );

    l2.put(&Key::new(shared_path, RecordKind::Lookup), &lookup)
        .unwrap();
    l2.put(&Key::new(shared_path, RecordKind::Attr), &attr)
        .unwrap();

    assert!(
        l2.get(&Key::new(shared_path, RecordKind::Lookup))
            .unwrap()
            .is_some()
    );
    assert!(
        l2.get(&Key::new(shared_path, RecordKind::Attr))
            .unwrap()
            .is_some()
    );
    assert!(
        l2.get(&Key::new(shared_path, RecordKind::Dirents))
            .unwrap()
            .is_none()
    );
}
