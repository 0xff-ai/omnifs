use omnifs_host::cache::{
    AttrPayload, CacheRecord, DirentRecord, DirentsPayload, EntryMeta, FileAttrsCache,
    LookupPayload, RecordKind, SCHEMA_VERSION,
};
use omnifs_host::omnifs::provider::types as wit_types;

fn exact_file(size: u64) -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size: wit_types::FileSize::Exact(size),
        bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
        stability: wit_types::Stability::Immutable,
        version_token: None,
    })
}

fn deferred_file(size: wit_types::FileSize) -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size,
        bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
        stability: wit_types::Stability::Immutable,
        version_token: None,
    })
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
fn cache_record_rejects_prior_schema_version() {
    // v5 collapsed cache re-skin types into WIT types. EntryMeta payloads
    // produced under v4 are byte-incompatible (the File variant now embeds
    // FileProj data instead of being a flat marker), so legacy records must
    // not be accepted by the v5 reader. The L2 read path goes through
    // `CacheRecord::deserialize`, which version-gates the record header.
    let mut bytes = CacheRecord {
        schema_version: SCHEMA_VERSION,
        kind: RecordKind::Lookup,
        payload: vec![],
    }
    .serialize();
    assert_eq!(SCHEMA_VERSION, 5, "schema version must be 5 in v5+");
    bytes[0] = 4; // pretend this is an old v4 record
    assert!(CacheRecord::deserialize(&bytes).is_none());
}

#[test]
fn non_exact_sizes_report_fuse_stat_values() {
    assert_eq!(deferred_file(wit_types::FileSize::NonZero).st_size(), 1);
    assert_eq!(deferred_file(wit_types::FileSize::Unknown).st_size(), 1);
}

#[test]
fn inline_file_payload_round_trip_through_wit_types() {
    // Phase 8.2 regression test: every payload that carries `FileAttrsCache`
    // must round-trip through postcard with the wit_types-backed shape. This
    // exercises the inline-bytes variant of ProjBytes which is the largest
    // change relative to the v4 schema.
    let meta = EntryMeta::file(FileAttrsCache {
        size: wit_types::FileSize::Exact(4),
        bytes: wit_types::ProjBytes::Inline(vec![0xde, 0xad, 0xbe, 0xef]),
        stability: wit_types::Stability::Immutable,
        version_token: Some("v1".to_string()),
    });

    let lookup_bytes = LookupPayload::Positive(meta.clone()).serialize().unwrap();
    let Some(LookupPayload::Positive(decoded)) = LookupPayload::deserialize(&lookup_bytes) else {
        panic!("expected positive lookup payload");
    };
    assert!(decoded.is_file());
    let attrs = decoded.attrs.expect("file should carry attrs");
    assert!(matches!(attrs.size, wit_types::FileSize::Exact(4)));
    assert!(matches!(attrs.stability, wit_types::Stability::Immutable));
    assert_eq!(attrs.version_token.as_deref(), Some("v1"));
    assert_eq!(attrs.inline_bytes(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));

    let attr_bytes = AttrPayload { meta: meta.clone() }.serialize().unwrap();
    let decoded = AttrPayload::deserialize(&attr_bytes).unwrap();
    assert!(decoded.meta.is_file());
    assert_eq!(decoded.meta.st_size(), 4);

    let dirents_bytes = DirentsPayload {
        entries: vec![DirentRecord {
            name: "blob".to_string(),
            meta,
        }],
        exhaustive: true,
    }
    .serialize()
    .unwrap();
    let decoded = DirentsPayload::deserialize(&dirents_bytes).unwrap();
    assert_eq!(decoded.entries.len(), 1);
    assert!(decoded.entries[0].meta.is_file());
}

#[test]
fn ranged_volatile_payload_round_trip_through_wit_types() {
    // Volatile stability requires ProjBytes::Deferred(ReadMode::Ranged).
    let meta = EntryMeta::file(FileAttrsCache {
        size: wit_types::FileSize::Unknown,
        bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Ranged),
        stability: wit_types::Stability::Volatile,
        version_token: None,
    });
    let bytes = AttrPayload { meta }.serialize().unwrap();
    let decoded = AttrPayload::deserialize(&bytes).unwrap();
    let attrs = decoded.meta.attrs.expect("file should carry attrs");
    assert!(matches!(attrs.stability, wit_types::Stability::Volatile));
    assert!(matches!(
        attrs.bytes,
        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Ranged)
    ));
}
