use crate::error::NfsFrontendError;
use crate::export::ReadOnlyExport;
use crate::protocol::filehandle::{generation, now_sec};
use crate::protocol::rpc::{handle_rpc_record, read_rpc_record, write_rpc_record};
use crate::trace::Trace;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub struct RunningNfsServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RunningNfsServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RunningNfsServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start_server(
    export: Arc<dyn ReadOnlyExport>,
    bind: SocketAddr,
    trace_path: Option<PathBuf>,
) -> Result<RunningNfsServer, NfsFrontendError> {
    let listener = TcpListener::bind(bind)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let trace = Trace::new(trace_path)?;
    let generation = generation();
    trace.line(format!(
        "ready addr={addr} generation={generation} boot_time={}",
        now_sec()
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let thread = thread::spawn(move || {
        while !thread_shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(error) = stream.set_nonblocking(false) {
                        trace.line(format!("connection_config_error peer={peer:?} err={error}"));
                        continue;
                    }
                    let export = Arc::clone(&export);
                    let trace = trace.clone();
                    trace.line(format!("connection peer={peer:?}"));
                    thread::spawn(move || {
                        if let Err(error) =
                            serve_connection(stream, generation, export.as_ref(), &trace)
                        {
                            trace.line(format!("connection_closed peer={peer:?} err={error}"));
                        }
                    });
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                },
                Err(error) => {
                    trace.line(format!("accept_error err={error}"));
                    break;
                },
            }
        }
    });

    Ok(RunningNfsServer {
        addr,
        shutdown,
        thread: Some(thread),
    })
}

fn serve_connection(
    mut stream: TcpStream,
    generation: u64,
    export: &dyn ReadOnlyExport,
    trace: &Trace,
) -> io::Result<()> {
    loop {
        let Some(record) = read_rpc_record(&mut stream)? else {
            return Ok(());
        };
        let response = handle_rpc_record(&record, generation, export, trace);
        write_rpc_record(&mut stream, &response)?;
    }
}
