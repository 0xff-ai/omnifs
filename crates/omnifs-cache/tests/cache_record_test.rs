use omnifs_cache::{Record, RecordKind, SCHEMA_VERSION};

#[test]
fn cache_record_rejects_unknown_schema_version() {
    let mut bytes = Record {
        schema_version: SCHEMA_VERSION,
        kind: RecordKind::File,
        payload: vec![],
    }
    .serialize();
    bytes[0] = 99; // corrupt schema version
    assert!(Record::deserialize(&bytes).is_none());
}

#[test]
fn cache_record_rejects_prior_schema_version() {
    // The durable read path version-gates on the record header byte: any record
    // whose leading schema byte differs from `SCHEMA_VERSION` is rejected as a
    // cache miss before its payload is decoded. That header gate is the actual
    // safety mechanism that retires byte-incompatible prior-schema records, so
    // this test exercises the gate directly by flipping only the header byte; it
    // does not assert payload-level decode behavior.
    let mut bytes = Record {
        schema_version: SCHEMA_VERSION,
        kind: RecordKind::Lookup,
        payload: vec![],
    }
    .serialize();
    assert_eq!(SCHEMA_VERSION, 7, "schema version must be 7 in v7+");
    bytes[0] = 6; // pretend this is an old record with a prior schema byte
    assert!(Record::deserialize(&bytes).is_none());
}
