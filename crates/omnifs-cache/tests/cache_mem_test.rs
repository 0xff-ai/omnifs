use omnifs_cache::view::{Cache, VIEW_MEM_SKIP_THRESHOLD};
use omnifs_cache::{Key, Record, RecordKind};
use omnifs_core::path::Path;

fn p(path: &str) -> Path {
    Path::parse(path).expect("test path must be valid")
}

#[test]
fn mem_invalidate_exact() {
    let cache = Cache::new();
    let key = Key::new(&p("/owner/repo/README.md"), RecordKind::File);
    let key_same_path_other_kind = Key::new(&p("/owner/repo/README.md"), RecordKind::Lookup);
    let record = Record::new(RecordKind::File, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 42]);
    cache.put(&key, &record);

    assert!(cache.get(&key).is_some());
    assert!(cache.get(&key_same_path_other_kind).is_none());

    cache.mem_invalidate(&key);
    assert!(cache.get(&key).is_none());
    assert!(cache.get(&key_same_path_other_kind).is_none());
}

#[test]
fn mem_invalidate_prefix() {
    let cache = Cache::new();
    let matching_prefix = p("/owner/repo");

    let paths = [
        Key::new(&p("/owner/repo"), RecordKind::Dirents),
        Key::new(&p("/owner/repo/readme.md"), RecordKind::File),
        Key::new(&p("/owner/repo/src/lib.rs"), RecordKind::Lookup),
        Key::new(&p("/other/repo"), RecordKind::File),
    ];

    for key in &paths {
        cache.put(key, &Record::new(key.kind, vec![8, 8, 8]));
    }

    cache.mem_invalidate_entries_if(move |k, _| k.path.has_prefix(&matching_prefix));

    assert!(cache.get(&paths[0]).is_none());
    assert!(cache.get(&paths[1]).is_none());
    assert!(cache.get(&paths[2]).is_none());
    assert!(cache.get(&paths[3]).is_some());
}

#[test]
fn mem_skips_oversized_records() {
    let cache = Cache::new();
    let key = Key::new(&p("/big.bin"), RecordKind::File);
    let big_payload = vec![0u8; VIEW_MEM_SKIP_THRESHOLD + 1];
    let record = Record::new(RecordKind::File, big_payload);

    cache.put(&key, &record);
    // Oversized records are silently skipped from the mem.
    assert!(cache.get(&key).is_none());
}
