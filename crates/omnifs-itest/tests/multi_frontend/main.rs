//! Multi-frontend acceptance: one daemon, several renderers, one shared
//! namespace.
//!
//! The daemon is a frontend registry. These lanes prove that a single daemon can
//! serve more than one renderer over one namespace, and that an invalidation
//! reaches every renderer.
//!
//! - `dual_frontend_serves_one_namespace` (Linux only): one daemon serving FUSE
//!   and NFS concurrently. The full conformance row table runs against BOTH
//!   roots, and cross-frontend byte identity is asserted. Skips on macOS, which
//!   is NFS-only.
//! - `invalidation_reaches_both_frontends_within_one_op`: a live-growth
//!   invalidation is observed as fresh content through every mount within a
//!   bounded poll. Linux runs the dual variant; macOS runs a single-NFS variant.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE`. The live-daemon helpers hold the
//! cross-process NFS serialization lock for the daemon's lifetime, so this target
//! never races another live-mount binary.

#![cfg(not(target_os = "wasi"))]

#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use omnifs_itest::live;
#[cfg(target_os = "linux")]
use omnifs_workspace::runtime_record::RuntimeRecord;

/// The live-growth file the test provider serves: `hello/live-log` grows one
/// 12-byte line per 500ms from its first read, capped well above what these
/// bounded polls need.
const LIVE_LOG: &str = "test/hello/live-log";

fn acceptance_gated() -> bool {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run multi-frontend acceptance");
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
#[test]
fn dual_frontend_serves_one_namespace() {
    use omnifs_itest::matrix::{self, Exec, ROWS};

    if !acceptance_gated() {
        return;
    }

    // One daemon, FUSE at mount_points[0] and NFS at mount_points[1], over one
    // shared namespace. The helper holds the NFS serial lock.
    let Some(daemon) = live::start_multi_frontend_daemon(&["fuse", "nfs"]) else {
        return;
    };

    // The runtime record names both served frontends.
    let record = RuntimeRecord::read(&daemon.record_path())
        .expect("read runtime record")
        .expect("runtime record present while serving");
    assert_eq!(
        record.frontends.len(),
        2,
        "the daemon must record both served frontends, got {:?}",
        record.frontends
    );

    let fuse_root = daemon.tree_root(0);
    let nfs_root = daemon.tree_root(1);

    // Run the full conformance table against BOTH roots through the shared matrix
    // machinery, one column each.
    let fuse_scratch = tempfile::tempdir().expect("fuse scratch");
    let nfs_scratch = tempfile::tempdir().expect("nfs scratch");
    let fuse_card = matrix::run_column(
        &Exec::Local {
            root: fuse_root.clone(),
            scratch: fuse_scratch.path().to_path_buf(),
        },
        &matrix::LINUX_FUSE_NATIVE,
        ROWS,
    );
    let nfs_card = matrix::run_column(
        &Exec::Local {
            root: nfs_root.clone(),
            scratch: nfs_scratch.path().to_path_buf(),
        },
        &matrix::LINUX_NFS_LOOPBACK,
        ROWS,
    );

    // Write both scorecards and print both tables before asserting, so a red run
    // still leaves evidence.
    for card in [&fuse_card, &nfs_card] {
        let path = matrix::write_scorecard(card);
        eprintln!("scorecard: {}", path.display());
    }
    eprintln!(
        "\n{}",
        matrix::render_table(&[fuse_card.clone(), nfs_card.clone()])
    );

    // Cross-frontend byte identity: the two roots project the same bytes.
    assert_bytes_identical(
        &fuse_root.join("hello/message"),
        &nfs_root.join("hello/message"),
        usize::MAX,
    );
    // A 256 KiB slice of the ranged file is identical across frontends.
    assert_bytes_identical(
        &fuse_root.join("hello/large-ranged"),
        &nfs_root.join("hello/large-ranged"),
        256 * 1024,
    );

    // Assert both columns last, so the byte-identity evidence is captured first.
    assert_column_honest(&fuse_card);
    assert_column_honest(&nfs_card);

    drop(daemon);
}

/// On macOS the dual FUSE+NFS lane does not apply (NFS-only); skip with a message
/// so the target still runs green there.
#[cfg(not(target_os = "linux"))]
#[test]
fn dual_frontend_serves_one_namespace() {
    if !acceptance_gated() {
        return;
    }
    eprintln!("skip: dual FUSE+NFS is Linux-only; macOS is NFS-only");
}

#[cfg(target_os = "linux")]
#[test]
fn invalidation_reaches_both_frontends_within_one_op() {
    if !acceptance_gated() {
        return;
    }
    let Some(daemon) = live::start_multi_frontend_daemon(&["fuse", "nfs"]) else {
        return;
    };
    // The live-follow pump emits an `AttrsChanged` invalidation on the shared
    // namespace event stream; both frontends subscribe independently, so each
    // sees the grown file. Assert fresh (grown) content through both mounts.
    assert_live_growth_visible(&daemon.mount_points[0]);
    assert_live_growth_visible(&daemon.mount_points[1]);
    drop(daemon);
}

/// macOS is NFS-only, so the single-frontend variant asserts the same
/// invalidation reaches the one NFS renderer.
#[cfg(not(target_os = "linux"))]
#[test]
fn invalidation_reaches_both_frontends_within_one_op() {
    if !acceptance_gated() {
        return;
    }
    let Some(daemon) = live::start_native_daemon() else {
        return;
    };
    assert_live_growth_visible(&daemon.mount_point);
    drop(daemon);
}

/// A renderer serves the projected tree from a different process than the
/// projection owner over the Omnifs VFS wire protocol. A
/// daemon serves its fixed local attach socket; an `omnifs-nfs` child
/// (the shipped out-of-process NFS runner) mounts NFS over an Omnifs VFS
/// wire-backed namespace. The full conformance row table runs against that
/// mount with the same expectations as the regular macOS NFS loopback lane,
/// scored as column `macos-nfs-wire`.
#[cfg(not(target_os = "linux"))]
#[test]
fn wire_frontend_nfs_parity() {
    use omnifs_itest::matrix::{self, Column, Exec, ROWS};

    if !acceptance_gated() {
        return;
    }

    let Some(daemon) = live::start_wire_frontend() else {
        return;
    };

    // The out-of-process NFS mount is a fresh column with the same expectations
    // as the regular macOS NFS loopback lane.
    let column = Column {
        id: "macos-nfs-wire",
        platform: "macos",
        expectations: matrix::MACOS_NFS_LOOPBACK.expectations,
    };
    let scratch = tempfile::tempdir().expect("wire scratch");
    let card = matrix::run_column(
        &Exec::Local {
            root: daemon.tree_root(),
            scratch: scratch.path().to_path_buf(),
        },
        &column,
        ROWS,
    );

    // Write and print the scorecard before asserting, so a red run leaves evidence.
    let path = matrix::write_scorecard(&card);
    eprintln!("scorecard: {}", path.display());
    eprintln!("\n{}", matrix::render_table(std::slice::from_ref(&card)));

    let mismatches = matrix::mismatches(&card);
    assert!(
        mismatches.is_empty(),
        "out-of-process wire column `{}` has {} expectation mismatch(es):\n  {}",
        card.column,
        mismatches.len(),
        mismatches.join("\n  ")
    );

    drop(daemon);
}

/// The wire-frontend live parity proof targets macOS NFS loopback; skip on Linux.
#[cfg(target_os = "linux")]
#[test]
fn wire_frontend_nfs_parity() {
    if !acceptance_gated() {
        return;
    }
    eprintln!("skip: the wire-frontend live parity test targets macOS NFS loopback");
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Assert the live-growth file at `<mount_point>/test/hello/live-log` delivers
/// fresh (grown) content through this mount within a bounded readiness poll: an
/// initial read, then a later read that returns strictly more bytes.
fn assert_live_growth_visible(mount_point: &Path) {
    let path = mount_point.join(LIVE_LOG);
    let baseline = read_len(&path).unwrap_or_else(|| {
        panic!(
            "live-log must be readable through {}",
            mount_point.display()
        )
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(len) = read_len(&path)
            && len > baseline
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "live-log at {} did not grow past {baseline} bytes within the poll window; \
             the invalidation did not reach this frontend",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The full byte length of `path`, read through the mount to EOF. `None` when the
/// file is not (yet) readable.
fn read_len(path: &Path) -> Option<usize> {
    std::fs::read(path).ok().map(|bytes| bytes.len())
}

/// Assert the first `limit` bytes of two files are byte-for-byte identical. Raw
/// byte comparison is byte identity (strictly stronger than equal digests), so
/// no hashing dependency is pulled in.
#[cfg(target_os = "linux")]
fn assert_bytes_identical(a: &Path, b: &Path, limit: usize) {
    let bytes_a = read_prefix(a, limit);
    let bytes_b = read_prefix(b, limit);
    assert!(
        !bytes_a.is_empty(),
        "expected non-empty bytes from {}",
        a.display()
    );
    assert_eq!(
        bytes_a.len(),
        bytes_b.len(),
        "byte length mismatch across frontends for {} vs {}",
        a.display(),
        b.display()
    );
    assert!(
        bytes_a == bytes_b,
        "cross-frontend byte identity failed for {} vs {}",
        a.display(),
        b.display()
    );
}

/// Read up to `limit` bytes from `path`.
#[cfg(target_os = "linux")]
fn read_prefix(path: &Path, limit: usize) -> Vec<u8> {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    let mut buf = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut buf)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    buf
}

/// Assert a conformance column has no expectation mismatch, printing the offenders.
#[cfg(target_os = "linux")]
fn assert_column_honest(card: &omnifs_itest::matrix::Scorecard) {
    let mismatches = omnifs_itest::matrix::mismatches(card);
    assert!(
        mismatches.is_empty(),
        "frontend column `{}` has {} expectation mismatch(es):\n  {}",
        card.column,
        mismatches.len(),
        mismatches.join("\n  ")
    );
}
