//! Wire-overhead measurement lane: the out-of-process frontend hop measured
//! against the in-process frontend on the conformance read/readdir workloads.
//!
//! Two lanes serve the same test-provider tree over NFS and run the same two
//! workloads; the only difference is where the frontend renders:
//!
//! - **Lane 1 (in-process).** The daemon serves the platform-default host-native
//!   frontend itself (the native path the conformance matrix uses).
//! - **Lane 2 (wire).** A namespace-only daemon (`--attach-socket`) plus an
//!   out-of-process `wire-test-frontend` NFS test double attached through the
//!   Omnifs VFS wire protocol, so every filesystem op crosses the VFS wire.
//!
//! The lanes run sequentially, never two mounts at once, under one held NFS
//! serial lock so no other test process interleaves a mount between them. Lane 1
//! is torn down fully (unmount, kill daemon, sweep) before lane 2 comes up, so
//! the measurements never overlap.
//!
//! The enforced budget is wire overhead under 15% on sequential 128 KiB reads and
//! under 25% on a readdir-heavy `find`. Evidence (all samples, medians, overhead)
//! always lands in `target/conformance/wire-perf.json` and a printed table before
//! the budget assertion, so a violation is diagnosable. This lane reports and
//! fails rather than changing its own budget.
//!
//! Gated on `OMNIFS_ACCEPTANCE_PERF`. Deliberately absent from every CI filter:
//! shared runners cannot hold a perf budget, so this is a local acceptance lane.

#![cfg(not(target_os = "wasi"))]

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use omnifs_itest::{live, matrix};
use serde::Serialize;

/// Sequential read size per iteration: 32 MiB, well inside `large-ranged`'s
/// 64 MiB, in 128 KiB chunks.
const READ_TARGET_BYTES: usize = 32 * 1024 * 1024;
const READ_CHUNK: usize = 128 * 1024;
/// Read is the expensive op, so fewer samples; find is cheap, so more.
const READ_ITERS: usize = 5;
const FIND_ITERS: usize = 7;
const READ_BUDGET_PCT: u32 = 15;
const FIND_BUDGET_PCT: u32 = 25;

/// Sequentially read `READ_TARGET_BYTES` from `<root>/hello/large-ranged` in
/// 128 KiB chunks and return the wall-clock. A direct read of one bounded file,
/// not a recursive traversal, so `hello/`'s unbounded fixtures are never touched.
fn time_read(root: &Path) -> Duration {
    let path = root.join("hello/large-ranged");
    let start = Instant::now();
    let mut file =
        File::open(&path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total = 0usize;
    while total < READ_TARGET_BYTES {
        let read = file.read(&mut buf).expect("read chunk");
        assert!(
            read != 0,
            "large-ranged ended at {total} bytes, short of {READ_TARGET_BYTES}"
        );
        total += read;
    }
    start.elapsed()
}

/// Time `find <root>/items -type f`: a readdir-heavy walk of a bounded, exhaustive
/// subtree (never `$ROOT` or `hello/`, whose fixtures never terminate).
fn time_find(root: &Path) -> Duration {
    let items = root.join("items");
    let start = Instant::now();
    let status = Command::new("find")
        .arg(&items)
        .args(["-type", "f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn find");
    let elapsed = start.elapsed();
    assert!(
        status.success(),
        "find {} -type f failed ({status})",
        items.display()
    );
    elapsed
}

struct LaneSamples {
    read_ms: Vec<f64>,
    find_ms: Vec<f64>,
}

/// Warm each workload once (untimed) so the engine's canonical-byte cache is
/// primed identically on both lanes, then take the timed samples.
fn measure_lane(root: &Path) -> LaneSamples {
    let _ = time_read(root);
    let _ = time_find(root);
    let read_ms: Vec<f64> = (0..READ_ITERS).map(|_| dur_ms(time_read(root))).collect();
    let find_ms: Vec<f64> = (0..FIND_ITERS).map(|_| dur_ms(time_find(root))).collect();
    LaneSamples { read_ms, find_ms }
}

fn dur_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The median of an odd-length sample set: the middle element after sorting.
fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

#[derive(Serialize)]
struct Report {
    version: u32,
    generated_at: String,
    machine: String,
    workloads: Vec<Workload>,
}

#[derive(Serialize)]
struct Workload {
    id: &'static str,
    inproc_ms: Vec<f64>,
    wire_ms: Vec<f64>,
    inproc_median_ms: f64,
    wire_median_ms: f64,
    overhead_pct: f64,
    budget_pct: u32,
}

fn workload(id: &'static str, inproc_ms: Vec<f64>, wire_ms: Vec<f64>, budget_pct: u32) -> Workload {
    let inproc_median_ms = median(&inproc_ms);
    let wire_median_ms = median(&wire_ms);
    let overhead_pct = (wire_median_ms - inproc_median_ms) / inproc_median_ms * 100.0;
    Workload {
        id,
        inproc_ms,
        wire_ms,
        inproc_median_ms,
        wire_median_ms,
        overhead_pct,
        budget_pct,
    }
}

fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

fn uname_ms() -> String {
    Command::new("uname")
        .arg("-ms")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )
}

fn print_table(report: &Report) {
    eprintln!("\nworkload      inproc(ms)   wire(ms)   overhead   budget");
    for w in &report.workloads {
        eprintln!(
            "{:<12} {:>10.1} {:>10.1} {:>9.1}% {:>7}%",
            w.id, w.inproc_median_ms, w.wire_median_ms, w.overhead_pct, w.budget_pct
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // linear two-lane bring-up, measure, report
fn wire_overhead_within_budget() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_PERF").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_PERF=1 to run the wire-overhead measurement lane");
        return;
    }
    if !live::platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        return;
    }

    // One NFS serial lock for the whole test: both lanes run sequentially under
    // it, so no other test process can interleave a mount between the two
    // measured bring-ups. The per-lane bring-up uses the `_holding_lock` variants
    // so neither lane acquires (or, on teardown, drops) its own copy.
    let _nfs_lock = live::nfs_serial_lock();

    // Lane 1 (in-process): the daemon serves the platform-default frontend.
    let inproc = {
        let Some(daemon) = live::start_native_daemon_holding_lock() else {
            eprintln!("skip: in-process lane could not come up");
            return;
        };
        let samples = measure_lane(&daemon.tree_root());
        // Full teardown (unmount, kill daemon, sweep) before lane 2 mounts, so the
        // two measurements never overlap.
        drop(daemon);
        samples
    };

    // Let the NFS teardown settle before the wire lane mounts a fresh export.
    std::thread::sleep(Duration::from_secs(1));

    // Lane 2 (wire): a namespace-only daemon plus an out-of-process nfs frontend.
    let wire = {
        let Some(wire_daemon) = live::start_wire_frontend_holding_lock() else {
            eprintln!("skip: wire lane could not come up");
            return;
        };
        let samples = measure_lane(&wire_daemon.tree_root());
        drop(wire_daemon);
        samples
    };

    let report = Report {
        version: 1,
        generated_at: now_rfc3339(),
        machine: uname_ms(),
        workloads: vec![
            workload("read-128k", inproc.read_ms, wire.read_ms, READ_BUDGET_PCT),
            workload(
                "find-readdir",
                inproc.find_ms,
                wire.find_ms,
                FIND_BUDGET_PCT,
            ),
        ],
    };

    // Evidence always lands before the assertion, so a blown budget is diagnosable.
    let path = matrix::scorecard_dir().join("wire-perf.json");
    let json = serde_json::to_string_pretty(&report).expect("serialize wire-perf report");
    std::fs::write(&path, json).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    eprintln!("wire-perf: {}", path.display());
    print_table(&report);

    for w in &report.workloads {
        assert!(
            w.overhead_pct <= f64::from(w.budget_pct),
            "workload `{}` wire overhead {:.1}% exceeds budget {}% \
             (in-process median {:.1}ms, wire median {:.1}ms). \
             Report the measurements rather than relaxing the budget.",
            w.id,
            w.overhead_pct,
            w.budget_pct,
            w.inproc_median_ms,
            w.wire_median_ms,
        );
    }
}
