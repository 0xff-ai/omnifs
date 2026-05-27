use thiserror::Error;

use crate::envelope::InspectorRecord;

#[derive(Debug, Error)]
pub enum ParseRecordError {
    #[error("empty line")]
    Empty,
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema version {found}, expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
}

/// Parse one complete JSONL line into a [`InspectorRecord`].
pub fn parse_record_line(line: &str) -> Result<InspectorRecord, ParseRecordError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(ParseRecordError::Empty);
    }
    parse_record(trimmed)
}

pub fn parse_record(json: &str) -> Result<InspectorRecord, ParseRecordError> {
    let record: InspectorRecord = serde_json::from_str(json)?;
    if record.v != crate::envelope::SCHEMA_VERSION {
        return Err(ParseRecordError::UnsupportedVersion {
            found: record.v,
            expected: crate::envelope::SCHEMA_VERSION,
        });
    }
    Ok(record)
}

/// Split a tail buffer into complete lines, leaving the trailing partial line in `remainder`.
pub fn split_complete_lines(buffer: &str) -> (Vec<&str>, &str) {
    let mut complete = Vec::new();
    let mut rest = buffer;
    while let Some(pos) = rest.find('\n') {
        let (line, after) = rest.split_at(pos);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.is_empty() {
            complete.push(line);
        }
        rest = &after[1..];
    }
    (complete, rest)
}

/// Deserialize every complete line; skip empty lines; return partial tail.
pub fn parse_complete_lines(buffer: &str) -> (Vec<InspectorRecord>, &str) {
    let (lines, remainder) = split_complete_lines(buffer);
    let mut records = Vec::with_capacity(lines.len());
    for line in lines {
        if let Ok(record) = parse_record_line(line) {
            records.push(record);
        }
    }
    (records, remainder)
}

pub fn serialize_record(record: &InspectorRecord) -> Result<String, serde_json::Error> {
    serde_json::to_string(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::InspectorRecord;
    use crate::event::{InspectorEvent, OpEnd};
    use crate::kind::CalloutKind;
    use crate::outcome::OutcomeFields;

    #[test]
    fn roundtrip_fuse_start_example() {
        let json = r#"{"v":1,"ts":"2026-05-23T12:14:08.123456Z","mono_us":123456789,"seq":0,"trace_id":42,"event":{"type":"fuse.start","op":"lookup","mount":"github","path":"/raulk/omnifs"}}"#;
        let record = parse_record(json).expect("parse");
        assert_eq!(record.mono_us, 123_456_789);
        assert_eq!(record.trace_id, 42);
        assert!(matches!(record.event, InspectorEvent::FuseStart { .. }));
        let again = serialize_record(&record).expect("serialize");
        let reparsed = parse_record(&again).expect("reparse");
        assert_eq!(record, reparsed);
    }

    #[test]
    fn fuse_end_flattens_outcome() {
        let record = InspectorRecord::new(
            "2026-05-23T12:14:09Z",
            200,
            1,
            InspectorEvent::FuseEnd {
                op: "lookup".to_string(),
                end: OpEnd {
                    elapsed_us: 3000,
                    result: OutcomeFields::ok(),
                },
            },
        );
        let json = serialize_record(&record).expect("serialize");
        assert!(json.contains("\"outcome\":\"ok\""));
        assert!(json.contains("\"type\":\"fuse.end\""));
        assert!(json.contains("\"trace_id\":1"));
    }

    #[test]
    fn callout_events_correlate_on_wire() {
        let start = InspectorRecord::new(
            "t",
            1,
            9,
            InspectorEvent::CalloutStart {
                operation_id: 3,
                callout_index: 0,
                kind: CalloutKind::GitOpenRepo,
                summary: "git.open_repo github.com:o/r".into(),
            },
        );
        let end = InspectorRecord::new(
            "t",
            2,
            9,
            InspectorEvent::CalloutEnd {
                operation_id: 3,
                callout_index: 0,
                end: OpEnd {
                    elapsed_us: 412_000,
                    result: OutcomeFields::ok(),
                },
            },
        );
        let start_json = serialize_record(&start).expect("start");
        let end_json = serialize_record(&end).expect("end");
        let start_parsed = parse_record(&start_json).expect("parse start");
        let end_parsed = parse_record(&end_json).expect("parse end");
        assert_eq!(start_parsed.trace_id, end_parsed.trace_id);
        match (start_parsed.event, end_parsed.event) {
            (
                InspectorEvent::CalloutStart {
                    operation_id: oa,
                    callout_index: ia,
                    ..
                },
                InspectorEvent::CalloutEnd {
                    operation_id: ob,
                    callout_index: ib,
                    ..
                },
            ) => {
                assert_eq!(oa, ob);
                assert_eq!(ia, ib);
            },
            _ => panic!("expected callout pair"),
        }
    }

    #[test]
    fn partial_tail_preserves_incomplete_line() {
        let buffer = "{\"v\":1,\"ts\":\"t\",\"mono_us\":1,\"seq\":0,\"trace_id\":1,\"event\":{\"type\":\"fuse.start\",\"op\":\"read\",\"mount\":\"dns\",\"path\":\"/\"}}\n{\"v\":1,";
        let (records, tail) = parse_complete_lines(buffer);
        assert_eq!(records.len(), 1);
        assert!(tail.starts_with("{\"v\":1,"));
    }

    #[test]
    fn rejects_unsupported_version() {
        let json = r#"{"v":99,"ts":"t","mono_us":0,"seq":0,"trace_id":1,"event":{"type":"fuse.start","op":"x","mount":"m","path":"/"}}"#;
        let err = parse_record(json).unwrap_err();
        assert!(matches!(err, ParseRecordError::UnsupportedVersion { .. }));
    }
}
