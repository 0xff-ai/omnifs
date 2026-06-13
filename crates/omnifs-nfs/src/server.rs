use crate::error::NfsFrontendError;
use crate::export::ReadOnlyExport;
use crate::protocol::client::ClientTable;
use crate::protocol::filehandle::{generation, now_sec};
use crate::protocol::rpc::{handle_rpc_record, read_rpc_record, write_rpc_record};
use crate::trace::Trace;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const MAX_CONNECTIONS: usize = 64;
const CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(5);

pub struct RunningNfsServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
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
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        join_workers(&self.workers);
    }
}

pub fn start_server(
    export: Arc<dyn ReadOnlyExport>,
    bind: SocketAddr,
    trace_path: Option<PathBuf>,
) -> Result<RunningNfsServer, NfsFrontendError> {
    if !bind.ip().is_loopback() {
        return Err(NfsFrontendError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("NFS loopback server must bind to a loopback address, got {bind}"),
        )));
    }

    let listener = TcpListener::bind(bind)?;
    let addr = listener.local_addr()?;
    let trace = Trace::new(trace_path)?;
    let generation = generation();
    let clients = Arc::new(ClientTable::new(generation));
    trace.line(&format!(
        "ready addr={addr} generation={generation} boot_time={}",
        now_sec()
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(Mutex::new(Vec::new()));
    let thread_workers = Arc::clone(&workers);
    let thread = thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if thread_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    prune_finished_workers(&thread_workers);
                    let Some(connection_permit) =
                        ConnectionPermit::try_acquire(Arc::clone(&active_connections))
                    else {
                        trace.line(&format!("connection_rejected peer={peer:?} reason=limit"));
                        continue;
                    };
                    if let Err(error) = stream.set_nonblocking(false) {
                        trace.line(&format!(
                            "connection_config_error peer={peer:?} err={error}"
                        ));
                        continue;
                    }
                    if let Err(error) = stream.set_read_timeout(Some(CONNECTION_IO_TIMEOUT)) {
                        trace.line(&format!(
                            "connection_config_error peer={peer:?} err={error}"
                        ));
                        continue;
                    }
                    if let Err(error) = stream.set_write_timeout(Some(CONNECTION_IO_TIMEOUT)) {
                        trace.line(&format!(
                            "connection_config_error peer={peer:?} err={error}"
                        ));
                        continue;
                    }
                    let export = Arc::clone(&export);
                    let clients = Arc::clone(&clients);
                    let trace = trace.clone();
                    trace.line(&format!("connection peer={peer:?}"));
                    let worker = thread::spawn(move || {
                        let _connection_permit = connection_permit;
                        if let Err(error) = serve_connection(
                            stream,
                            generation,
                            clients.as_ref(),
                            export.as_ref(),
                            &trace,
                        ) {
                            trace.line(&format!("connection_closed peer={peer:?} err={error}"));
                        }
                    });
                    thread_workers.lock().expect("workers lock").push(worker);
                },
                Err(error) => {
                    if thread_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    trace.line(&format!("accept_error err={error}"));
                    break;
                },
            }
        }
        join_workers(&thread_workers);
    });

    Ok(RunningNfsServer {
        addr,
        shutdown,
        workers,
        thread: Some(thread),
    })
}

fn serve_connection(
    mut stream: TcpStream,
    generation: u64,
    clients: &ClientTable,
    export: &dyn ReadOnlyExport,
    trace: &Trace,
) -> io::Result<()> {
    loop {
        let record = match read_rpc_record(&mut stream) {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                trace.line("connection_idle_timeout");
                return Ok(());
            },
            Err(error) => return Err(error),
        };
        let response = handle_rpc_record(&record, generation, clients, export, trace);
        write_rpc_record(&mut stream, &response)?;
    }
}

struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl ConnectionPermit {
    fn try_acquire(active: Arc<AtomicUsize>) -> Option<Self> {
        let mut current = active.load(Ordering::Acquire);
        loop {
            if current >= MAX_CONNECTIONS {
                return None;
            }
            match active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self { active }),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn prune_finished_workers(workers: &Mutex<Vec<thread::JoinHandle<()>>>) {
    let mut workers = workers.lock().expect("workers lock");
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}

fn join_workers(workers: &Mutex<Vec<thread::JoinHandle<()>>>) {
    let workers = std::mem::take(&mut *workers.lock().expect("workers lock"));
    for worker in workers {
        let _ = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{Attr, DirListing, OpenRead, OpenResult, StateId, Status, StatusResult};

    struct EmptyExport;

    impl ReadOnlyExport for EmptyExport {
        fn root(&self) -> u64 {
            1
        }

        fn attr(&self, _id: u64) -> StatusResult<Attr> {
            Err(Status::Stale)
        }

        fn lookup(&self, _parent: u64, _name: &str) -> StatusResult<u64> {
            Err(Status::NoEnt)
        }

        fn readdir(&self, _id: u64) -> StatusResult<DirListing> {
            Err(Status::NotDir)
        }

        fn read(&self, _id: u64) -> StatusResult<Vec<u8>> {
            Err(Status::Invalid)
        }

        fn readlink(&self, _id: u64) -> StatusResult<Vec<u8>> {
            Err(Status::Invalid)
        }

        fn open_state(
            &self,
            _generation: u64,
            _id: u64,
            _clientid: u64,
            _access: u32,
        ) -> StatusResult<OpenResult> {
            Err(Status::Invalid)
        }

        fn validate_state(&self, _stateid: StateId) -> StatusResult<()> {
            Err(Status::BadStateId)
        }

        fn read_state(
            &self,
            _stateid: StateId,
            _offset: u64,
            _count: u32,
        ) -> StatusResult<OpenRead> {
            Err(Status::BadStateId)
        }

        fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
            Ok(stateid)
        }

        fn renew_client(&self, _clientid: u64) -> StatusResult<()> {
            Ok(())
        }
    }

    #[test]
    fn start_server_rejects_non_loopback_bind() {
        let bind = "0.0.0.0:0".parse().expect("bind addr");
        let result = start_server(Arc::new(EmptyExport), bind, None);
        assert!(matches!(result, Err(NfsFrontendError::Io(_))));
    }
}
