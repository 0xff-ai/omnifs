use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::events::InspectorLine;

#[derive(Debug, Error)]
pub enum LineWriteError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Append newline-framed typed Inspector lines.
pub struct InspectorLineWriter {
    path: PathBuf,
    file: File,
}

impl InspectorLineWriter {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LineWriteError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_line(&mut self, line: &InspectorLine) -> Result<(), LineWriteError> {
        let line = line.to_json_line()?;
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::InspectorLine;
    use crate::events::envelope::InspectorRecord;
    use crate::events::event::InspectorEvent;

    #[test]
    fn writer_appends_newline_framed_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("inspector.jsonl");
        let mut writer = InspectorLineWriter::open(&path).expect("open");
        writer
            .write_line(&InspectorLine::Record(InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                1,
                7,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/a".into(),
                },
            )))
            .expect("write record");
        writer
            .write_line(&InspectorLine::Dropped { count: 3 })
            .expect("write");

        let contents = std::fs::read_to_string(path).expect("read");
        let parsed = contents
            .lines()
            .map(InspectorLine::parse_line)
            .collect::<Result<Vec<_>, _>>()
            .expect("parse every line");
        assert_eq!(parsed.len(), 2);
        let InspectorLine::Record(record) = &parsed[0] else {
            panic!("expected record")
        };
        assert_eq!(record.trace_id, 7);
        assert!(matches!(record.event, InspectorEvent::FuseStart { .. }));
        assert_eq!(parsed[1], InspectorLine::Dropped { count: 3 });
    }
}
