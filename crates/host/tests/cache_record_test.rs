use omnifs_host::cache::{
    AttrPayload, BytesCache, CacheRecord, DirentRecord, DirentsPayload, EntryKindCache, EntryMeta,
    FileAttrsCache, LookupPayload, ReadModeCache, RecordKind, SCHEMA_VERSION, SizeCache,
    StabilityCache,
};

fn exact_file(size: u64) -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size: SizeCache::Exact(size),
        bytes: BytesCache::Deferred(ReadModeCache::Full),
        stability: StabilityCache::Immutable,
        version_token: None,
    })
}

fn deferred_file(size: SizeCache) -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size,
        bytes: BytesCache::Deferred(ReadModeCache::Full),
        stability: StabilityCache::Immutable,
        version_token: None,
    })
}

#[test]
fn cache_record_round_trip() {
    let record = CacheRecord {
        schema_version: SCHEMA_VERSION,
        kind: RecordKind::Attr,
        payload: vec![1, 2, 3, 4],
    };
    let bytes = record.serialize();
    let decoded = CacheRecord::deserialize(&bytes).unwrap();
    assert_eq!(decoded.schema_version, SCHEMA_VERSION);
    assert_eq!(decoded.kind, RecordKind::Attr);
    assert_eq!(decoded.payload, vec![1, 2, 3, 4]);
}

#[test]
fn cache_record_rejects_unknown_schema_version() {
    let mut bytes = CacheRecord {
        schema_version: SCHEMA_VERSION,
        kind: RecordKind::File,
        payload: vec![],
    }
    .serialize();
    bytes[0] = 99; // corrupt schema version
    assert!(CacheRecord::deserialize(&bytes).is_none());
}

#[test]
fn lookup_payload_positive_round_trip() {
    let payload = LookupPayload::Positive(exact_file(42));
    let bytes = payload.serialize().unwrap();
    let decoded = LookupPayload::deserialize(&bytes).unwrap();
    let LookupPayload::Positive(meta) = decoded else {
        panic!("expected positive lookup payload");
    };
    assert_eq!(meta.kind, EntryKindCache::File);
    assert_eq!(meta.st_size(), 42);
}

#[test]
fn lookup_payload_negative_round_trip() {
    let bytes = LookupPayload::Negative.serialize().unwrap();
    let decoded = LookupPayload::deserialize(&bytes).unwrap();
    assert!(matches!(decoded, LookupPayload::Negative));
}

#[test]
fn attr_payload_round_trip() {
    let payload = AttrPayload {
        meta: EntryMeta::directory(),
    };
    let bytes = payload.serialize().unwrap();
    let decoded = AttrPayload::deserialize(&bytes).unwrap();
    assert_eq!(decoded.meta.kind, EntryKindCache::Directory);
    assert_eq!(decoded.meta.st_size(), 0);
}

#[test]
fn non_exact_sizes_report_fuse_stat_values() {
    assert_eq!(deferred_file(SizeCache::NonZero).st_size(), 1);
    assert_eq!(deferred_file(SizeCache::Unknown).st_size(), 0);
}

#[test]
fn dirents_payload_round_trip() {
    let payload = DirentsPayload {
        entries: vec![
            DirentRecord {
                name: "title".to_string(),
                meta: exact_file(128),
            },
            DirentRecord {
                name: "comments".to_string(),
                meta: EntryMeta::directory(),
            },
        ],
        exhaustive: true,
    };
    let bytes = payload.serialize().unwrap();
    let decoded = DirentsPayload::deserialize(&bytes).unwrap();
    assert_eq!(decoded.entries.len(), 2);
    assert_eq!(decoded.entries[0].name, "title");
    assert_eq!(decoded.entries[0].meta.st_size(), 128);
    assert_eq!(decoded.entries[1].name, "comments");
    assert_eq!(decoded.entries[1].meta.kind, EntryKindCache::Directory);
}
