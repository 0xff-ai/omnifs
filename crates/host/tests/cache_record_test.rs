use omnifs_host::cache::{
    AttrPayload, CacheRecord, DirentRecord, DirentsPayload, EntryKindCache, LookupPayload,
    RecordKind, SCHEMA_VERSION,
};

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
    let payload = LookupPayload::Positive {
        kind: EntryKindCache::File,
        size: 42,
    };
    let bytes = payload.serialize().unwrap();
    let decoded = LookupPayload::deserialize(&bytes).unwrap();
    assert!(matches!(
        decoded,
        LookupPayload::Positive {
            kind: EntryKindCache::File,
            size: 42
        }
    ));
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
        kind: EntryKindCache::Directory,
        size: 0,
    };
    let bytes = payload.serialize().unwrap();
    let decoded = AttrPayload::deserialize(&bytes).unwrap();
    assert_eq!(decoded.kind, EntryKindCache::Directory);
    assert_eq!(decoded.size, 0);
}

#[test]
fn dirents_payload_round_trip() {
    let payload = DirentsPayload {
        entries: vec![
            DirentRecord {
                name: "title".to_string(),
                kind: EntryKindCache::File,
                size: 128,
            },
            DirentRecord {
                name: "comments".to_string(),
                kind: EntryKindCache::Directory,
                size: 0,
            },
        ],
        exhaustive: true,
    };
    let bytes = payload.serialize().unwrap();
    let decoded = DirentsPayload::deserialize(&bytes).unwrap();
    assert_eq!(decoded.entries.len(), 2);
    assert_eq!(decoded.entries[0].name, "title");
    assert_eq!(decoded.entries[0].size, 128);
    assert_eq!(decoded.entries[1].name, "comments");
    assert_eq!(decoded.entries[1].kind, EntryKindCache::Directory);
}
