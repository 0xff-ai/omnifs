use omnifs_cache::view::{Cache, VIEW_MEM_SKIP_THRESHOLD};
use omnifs_cache::{Key, Record, RecordKind};

#[test]
fn mem_invalidate_exact() {
    let cache = Cache::new();
    let key = Key::new("/owner/repo/README.md", RecordKind::File);
    let key_same_path_other_kind = Key::new("/owner/repo/README.md", RecordKind::Lookup);
    let record = Record::new(RecordKind::File, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 42]);
    cache.put(&key, &record);

    assert!(cache.get(&key).is_some());
    assert!(cache.get(&key_same_path_other_kind).is_none());

    cache.invalidate(&key);
    assert!(cache.get(&key).is_none());
    assert!(cache.get(&key_same_path_other_kind).is_none());
}

#[test]
fn mem_invalidate_prefix() {
    let cache = Cache::new();
    let matching_prefix = "/owner/repo".to_string();

    let paths = [
        Key::new("/owner/repo", RecordKind::Dirents),
        Key::new("/owner/repo/readme.md", RecordKind::File),
        Key::new("/owner/repo/src/lib.rs", RecordKind::Lookup),
        Key::new("/other/repo", RecordKind::File),
    ];

    for key in &paths {
        cache.put(key, &Record::new(key.kind, vec![8, 8, 8]));
    }

    cache.invalidate_entries_if(move |k, _| {
        if k.path == matching_prefix {
            true
        } else {
            k.path.starts_with(&matching_prefix)
                && k.path.as_bytes().get(matching_prefix.len()) == Some(&b'/')
        }
    });

    assert!(cache.get(&paths[0]).is_none());
    assert!(cache.get(&paths[1]).is_none());
    assert!(cache.get(&paths[2]).is_none());
    assert!(cache.get(&paths[3]).is_some());
}

#[test]
fn mem_skips_oversized_records() {
    let cache = Cache::new();
    let key = Key::new("big.bin", RecordKind::File);
    let big_payload = vec![0u8; VIEW_MEM_SKIP_THRESHOLD + 1];
    let record = Record::new(RecordKind::File, big_payload);

    cache.put(&key, &record);
    // Oversized records are silently skipped from the mem.
    assert!(cache.get(&key).is_none());
}
