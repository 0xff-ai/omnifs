use crate::error::NfsFrontendError;
use crate::export::ReadOnlyExport;
use crate::protocol::filehandle::now_sec;
use crate::protocol::rpc::{handle_rpc_record, read_rpc_record, write_rpc_record};
use crate::trace::Trace;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const MAX_CONNECTIONS: usize = 64;
const CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on concurrently-dispatched RPC handler threads across all
/// connections. A provider-backed `readdir`/`lookup`/`read` can block on cold
/// upstream work (a GitHub fetch, a git clone); dispatching each RPC to its own
/// worker lets many run at once so one slow op never head-of-line blocks the
/// rest of a connection's traffic. The cap bounds total in-flight worker
/// threads under a request flood.
const MAX_INFLIGHT_RPCS: usize = 128;

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
    trace.line(&format!(
        "ready addr={addr} generation={} boot_time={}",
        export.generation(),
        now_sec()
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(Mutex::new(Vec::new()));
    let thread_workers = Arc::clone(&workers);
    let slots = RpcSlots::new(MAX_INFLIGHT_RPCS);
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
                    let slots = Arc::clone(&slots);
                    let trace = trace.clone();
                    let conn_trace = trace.clone();
                    trace.line(&format!("connection peer={peer:?}"));
                    let worker = thread::spawn(move || {
                        let _connection_permit = connection_permit;
                        if let Err(error) = serve_connection(stream, &export, &conn_trace, &slots) {
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

/// Serve one client connection with concurrent RPC dispatch.
///
/// The read side keeps `stream` and, for each record, hands the work to a
/// dedicated handler thread instead of running it inline. A slow
/// provider-backed op therefore no longer blocks the next RPC on the same
/// connection (NFSv4.0 funnels a mount's traffic over one or a few
/// connections, so inline blocking wedged the whole mount). Replies carry their
/// own XID, so out-of-order completion is protocol-legal; a single writer
/// thread owns a cloned socket handle and frames each reply back as its handler
/// finishes, serializing the wire without serializing the work.
fn serve_connection(
    mut stream: TcpStream,
    export: &Arc<dyn ReadOnlyExport>,
    trace: &Trace,
    slots: &Arc<RpcSlots>,
) -> io::Result<()> {
    let write_stream = stream.try_clone()?;
    let (responses, rx) = mpsc::channel::<Vec<u8>>();
    let writer = thread::Builder::new()
        .name("nfs-writer".to_string())
        .spawn(move || {
            let mut write_stream = write_stream;
            while let Ok(payload) = rx.recv() {
                if write_rpc_record(&mut write_stream, &payload).is_err() {
                    break;
                }
            }
        })
        .expect("spawn nfs writer thread");

    // Per-connection count of dispatched-but-unfinished handlers. The idle read
    // timeout reaps the connection only when this is zero; otherwise the
    // connection stays open so an outstanding slow handler can still deliver its
    // reply here.
    let inflight = Arc::new(AtomicUsize::new(0));

    let outcome = loop {
        match read_rpc_record(&mut stream) {
            Ok(Some(record)) => {
                // Backpressure: block the reader when the global dispatch cap is
                // saturated rather than spawning unboundedly.
                let slot = slots.acquire();
                inflight.fetch_add(1, Ordering::AcqRel);
                let guard = InflightGuard {
                    _slot: slot,
                    inflight: Arc::clone(&inflight),
                };
                let responses = responses.clone();
                let export = Arc::clone(export);
                let handler_trace = trace.clone();
                let spawned = thread::Builder::new()
                    .name("nfs-rpc".to_string())
                    .spawn(move || {
                        let _guard = guard;
                        let response = handle_rpc_record(&record, &*export, &handler_trace);
                        // The writer may have already exited on a broken socket;
                        // a dropped reply is recovered by client retransmit.
                        let _ = responses.send(response);
                    });
                if spawned.is_err() {
                    // Guard/sender drop here, releasing the slot and inflight
                    // count; the client retransmits the unanswered call.
                    trace.line("rpc_dispatch_spawn_failed");
                }
            },
            Ok(None) => break Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if inflight.load(Ordering::Acquire) == 0 {
                    trace.line("connection_idle_timeout");
                    break Ok(());
                }
            },
            Err(error) => break Err(error),
        }
    };

    // Drop the reader's keep-alive sender so the writer exits once every
    // outstanding handler has sent its reply and dropped its own sender, then
    // wait for the wire to drain before tearing the connection down.
    drop(responses);
    let _ = writer.join();
    outcome
}

/// A global counting semaphore bounding concurrent RPC handler threads. A
/// permit is held for the lifetime of one dispatched RPC and released on drop.
struct RpcSlots {
    available: Mutex<usize>,
    free: Condvar,
}

impl RpcSlots {
    fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            available: Mutex::new(permits),
            free: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> RpcSlot {
        let mut available = self.available.lock().expect("rpc slots lock");
        while *available == 0 {
            available = self.free.wait(available).expect("rpc slots wait");
        }
        *available -= 1;
        RpcSlot {
            slots: Arc::clone(self),
        }
    }
}

struct RpcSlot {
    slots: Arc<RpcSlots>,
}

impl Drop for RpcSlot {
    fn drop(&mut self) {
        *self.slots.available.lock().expect("rpc slots lock") += 1;
        self.slots.free.notify_one();
    }
}

/// Held by a dispatched handler thread; on drop it releases the global slot and
/// decrements the connection's in-flight count.
struct InflightGuard {
    _slot: RpcSlot,
    inflight: Arc<AtomicUsize>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
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
        fn generation(&self) -> u64 {
            1
        }

        fn set_clientid(&self, _verifier: [u8; 8], _owner: Vec<u8>) -> (u64, [u8; 8]) {
            (0, [0; 8])
        }

        fn confirm_client(&self, _clientid: u64, _verifier: &[u8]) -> StatusResult<()> {
            Err(Status::StaleClientId)
        }

        fn client_confirmed(&self, _clientid: u64) -> bool {
            false
        }

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

        fn open_state(&self, _id: u64, _clientid: u64, _access: u32) -> StatusResult<OpenResult> {
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
