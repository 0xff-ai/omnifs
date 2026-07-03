use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::events::envelope::InspectorRecord;

#[derive(Debug, Error)]
pub enum LineWriteError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Append newline-framed JSONL inspector records.
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

    pub fn write_record(&mut self, record: &InspectorRecord) -> Result<(), LineWriteError> {
        let mut line = record.to_json()?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::envelope::InspectorRecord;
    use crate::events::event::InspectorEvent;

    #[test]
    fn writer_appends_newline_framed_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("inspector.jsonl");
        let mut writer = InspectorLineWriter::open(&path).expect("open");
        writer
            .write_record(&InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                1,
                7,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/a".into(),
                },
            ))
            .expect("write");

        let contents = std::fs::read_to_string(path).expect("read");
        let line = contents.lines().next().expect("one line");
        let parsed = InspectorRecord::parse_line(line).expect("parse");
        assert_eq!(parsed.trace_id, 7);
        assert!(matches!(parsed.event, InspectorEvent::FuseStart { .. }));
    }
}
