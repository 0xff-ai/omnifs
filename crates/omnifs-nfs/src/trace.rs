use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;

#[derive(Clone)]
pub(crate) struct Trace {
    inner: Arc<TraceInner>,
}

struct TraceInner {
    writer: Mutex<Option<mpsc::Sender<String>>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Trace {
    pub(crate) fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self {
                inner: Arc::new(TraceInner {
                    writer: Mutex::new(None),
                    thread: Mutex::new(None),
                }),
            });
        };

        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let (sender, receiver) = mpsc::channel::<String>();
        let thread = thread::spawn(move || {
            while let Ok(line) = receiver.recv() {
                let _ = writeln!(file, "{line}");
            }
        });
        Ok(Self {
            inner: Arc::new(TraceInner {
                writer: Mutex::new(Some(sender)),
                thread: Mutex::new(Some(thread)),
            }),
        })
    }

    pub(crate) fn line(&self, line: &str) {
        if let Some(writer) = self
            .inner
            .writer
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            let _ = writer.send(line.to_string());
        } else {
            tracing::debug!(target: "omnifs_nfs", "{line}");
        }
    }
}

impl Drop for TraceInner {
    fn drop(&mut self) {
        let _ = self.writer.get_mut().ok().and_then(Option::take);
        if let Some(thread) = self.thread.get_mut().ok().and_then(Option::take) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_trace_writer_flushes_on_drop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nfs.trace");
        {
            let trace = Trace::new(Some(path.clone())).expect("trace writer");
            trace.line("rpc xid=1 test");
        }

        let content = std::fs::read_to_string(path).expect("trace file");
        assert!(content.contains("rpc xid=1 test"));
    }
}
