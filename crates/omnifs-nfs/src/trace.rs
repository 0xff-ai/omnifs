use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Trace {
    file: Option<Arc<Mutex<File>>>,
}

impl Trace {
    pub(crate) fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let file = path
            .map(|path| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map(|file| Arc::new(Mutex::new(file)))
            })
            .transpose()?;
        Ok(Self { file })
    }

    pub(crate) fn line(&self, line: impl AsRef<str>) {
        if let Some(file) = &self.file {
            let mut file = file.lock();
            let _ = writeln!(file, "{}", line.as_ref());
        } else {
            tracing::debug!(target: "omnifs_nfs", "{}", line.as_ref());
        }
    }
}
