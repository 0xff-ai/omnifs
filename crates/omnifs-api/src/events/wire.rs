use thiserror::Error;

use crate::events::{InspectorLine, SCHEMA_VERSION};

#[derive(Debug, Error)]
pub enum ParseLineError {
    #[error("empty line")]
    Empty,
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema version {found}, expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
}

impl InspectorLine {
    /// Parse one complete JSONL line and validate its nested record schema.
    pub fn parse_line(line: &str) -> Result<Self, ParseLineError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(ParseLineError::Empty);
        }
        let line: Self = serde_json::from_str(trimmed)?;
        if let Self::Record(record) = &line
            && record.v != SCHEMA_VERSION
        {
            return Err(ParseLineError::UnsupportedVersion {
                found: record.v,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(line)
    }

    /// Serialize one typed Inspector line with its canonical newline framing.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self).map(|mut line| {
            line.push('\n');
            line
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::envelope::InspectorRecord;
    use crate::events::event::{InspectorEvent, OpEnd};
    use crate::events::kind::CalloutKind;
    use crate::events::outcome::OutcomeFields;

    #[test]
    fn roundtrip_fuse_start_example() {
        let json = r#"{"type":"record","value":{"v":1,"ts":"2026-05-23T12:14:08.123456Z","mono_us":123456789,"seq":0,"trace_id":42,"event":{"type":"fuse.start","op":"lookup","mount":"github","path":"/raulk/omnifs"}}}"#;
        let line = InspectorLine::parse_line(json).expect("parse");
        let InspectorLine::Record(record) = &line else {
            panic!("expected record line")
        };
        assert_eq!(record.mono_us, 123_456_789);
        assert_eq!(record.trace_id, 42);
        assert!(matches!(record.event, InspectorEvent::FuseStart { .. }));
        let again = line.to_json_line().expect("serialize");
        let reparsed = InspectorLine::parse_line(&again).expect("reparse");
        assert_eq!(line, reparsed);
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
        let json = InspectorLine::Record(record)
            .to_json_line()
            .expect("serialize");
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
        let start_json = InspectorLine::Record(start).to_json_line().expect("start");
        let end_json = InspectorLine::Record(end).to_json_line().expect("end");
        let InspectorLine::Record(start_parsed) =
            InspectorLine::parse_line(&start_json).expect("parse start")
        else {
            panic!("expected start record")
        };
        let InspectorLine::Record(end_parsed) =
            InspectorLine::parse_line(&end_json).expect("parse end")
        else {
            panic!("expected end record")
        };
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
        let buffer = "{\"type\":\"record\",\"value\":{\"v\":1,\"ts\":\"t\",\"mono_us\":1,\"seq\":0,\"trace_id\":1,\"event\":{\"type\":\"fuse.start\",\"op\":\"read\",\"mount\":\"dns\",\"path\":\"/\"}}}\n{\"type\":\"record\",\"value\":{\"v\":1,";
        let (lines, tail) = split_complete_lines(buffer);
        assert_eq!(lines.len(), 1);
        InspectorLine::parse_line(lines[0]).expect("complete line parses");
        assert!(tail.starts_with("{\"type\":\"record\""));
    }

    #[test]
    fn rejects_unsupported_version() {
        let json = r#"{"type":"record","value":{"v":99,"ts":"t","mono_us":0,"seq":0,"trace_id":1,"event":{"type":"fuse.start","op":"x","mount":"m","path":"/"}}}"#;
        let err = InspectorLine::parse_line(json).unwrap_err();
        assert!(matches!(err, ParseLineError::UnsupportedVersion { .. }));
    }

    #[test]
    fn rejects_blank_and_accepts_dropped_lines() {
        assert!(matches!(
            InspectorLine::parse_line("  \n"),
            Err(ParseLineError::Empty)
        ));
        assert_eq!(
            InspectorLine::parse_line(r#"{"type":"dropped","value":{"count":3}}"#)
                .expect("dropped line"),
            InspectorLine::Dropped { count: 3 }
        );
        assert!(InspectorLine::parse_line("# dropped 3 events").is_err());
        assert!(InspectorLine::parse_line(r#"{"v":1}"#).is_err());
    }
}
