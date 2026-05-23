use omnifs_host::cache::l0::Cache;
use omnifs_host::cache::{CacheRecord, Key, L0_SKIP_THRESHOLD, RecordKind};

#[test]
fn l0_invalidate_exact() {
    let l0 = Cache::new();
    let key = Key::new("owner/repo/README.md", RecordKind::File);
    let key_same_path_other_kind = Key::new("owner/repo/README.md", RecordKind::Lookup);
    let record = CacheRecord::new(RecordKind::File, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 42]);
    l0.put(key.clone(), record.clone());

    assert!(l0.get(&key).is_some());
    assert!(l0.get(&key_same_path_other_kind).is_none());

    l0.invalidate(&key);
    assert!(l0.get(&key).is_none());
    assert!(l0.get(&key_same_path_other_kind).is_none());
}

#[test]
fn l0_invalidate_prefix() {
    let l0 = Cache::new();
    let matching_prefix = "owner/repo".to_string();

    let paths = [
        Key::new("owner/repo", RecordKind::Dirents),
        Key::new("owner/repo/readme.md", RecordKind::File),
        Key::new("owner/repo/src/lib.rs", RecordKind::Lookup),
        Key::new("other/repo", RecordKind::File),
    ];

    for key in &paths {
        l0.put(key.clone(), CacheRecord::new(key.kind, vec![8, 8, 8]));
    }

    l0.invalidate_entries_if(move |k, _| {
        if k.path == matching_prefix {
            true
        } else {
            k.path.starts_with(&matching_prefix)
                && k.path.as_bytes().get(matching_prefix.len()) == Some(&b'/')
        }
    });

    assert!(l0.get(&paths[0]).is_none());
    assert!(l0.get(&paths[1]).is_none());
    assert!(l0.get(&paths[2]).is_none());
    assert!(l0.get(&paths[3]).is_some());
}

#[test]
fn l0_skips_oversized_records() {
    let l0 = Cache::new();
    let key = Key::new("big.bin", RecordKind::File);
    let big_payload = vec![0u8; L0_SKIP_THRESHOLD + 1];
    let record = CacheRecord::new(RecordKind::File, big_payload);

    l0.put(key.clone(), record);
    // Oversized records are silently skipped
    assert!(l0.get(&key).is_none());
}
