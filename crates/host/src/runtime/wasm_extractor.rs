//! Sandboxed archive extraction via an embedded Wasmtime component.
//!
//! The host ships a precompiled `omnifs-archive-extractor.wasm` (built
//! from `crates/omnifs-archive-extractor`) and runs it in a fresh
//! `wasmtime::Store` for each `open-archive` callout. The component
//! sees only:
//!
//! - `/blob/blob.dat` — read-only preopen of the archive bytes.
//! - `/out/` — read-write preopen of the destination directory.
//!
//! plus an engine-enforced fuel and memory cap. Anything else (the
//! host filesystem, sockets, env, time) is unreachable. Trips return
//! a typed [`ExtractError`]; the host translates them into the
//! existing `ArchiveError` shape so the rest of the runtime is
//! oblivious to the sandbox.

use crate::extractor_bindings::Extractor;
use crate::extractor_bindings::exports::omnifs::archive_extractor::extract as wit_extract;
use std::path::Path;
use wasmtime::component::{Component, InstancePre, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Embedded extractor wasm artifact. The path resolves relative to
/// `crates/host/src/`. `just build-providers` (and the `Extractor`
/// build-dep machinery) ensure this file exists before the host
/// crate compiles.
const EXTRACTOR_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/omnifs_archive_extractor.wasm"
));

/// Defaults the host applies if an `ArchiveExecutor` doesn't override.
/// Tuned to comfortably cover crates.io-class tarballs (median <1 MiB,
/// largest a few hundred MiB) while still rejecting zip-bombs.
pub const DEFAULT_LIMITS: ExtractorLimits = ExtractorLimits {
    max_entries: 50_000,
    max_file_size: 256 * 1024 * 1024,
    max_total_bytes: 1024 * 1024 * 1024,
    max_path_depth: 64,
    max_path_len: 4096,
    fuel: 5_000_000_000,
    max_memory_bytes: 256 * 1024 * 1024,
};

#[derive(Clone, Copy, Debug)]
pub struct ExtractorLimits {
    pub max_entries: u64,
    pub max_file_size: u64,
    pub max_total_bytes: u64,
    pub max_path_depth: u32,
    pub max_path_len: u32,
    pub fuel: u64,
    pub max_memory_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    TarGz,
    Tar,
    Zip,
}

impl From<ArchiveFormat> for wit_extract::ArchiveFormat {
    fn from(f: ArchiveFormat) -> Self {
        match f {
            ArchiveFormat::TarGz => Self::TarGz,
            ArchiveFormat::Tar => Self::Tar,
            ArchiveFormat::Zip => Self::Zip,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("too many entries")]
    TooManyEntries,
    #[error("file too large: {0}")]
    FileTooLarge(String),
    #[error("total bytes exceeded")]
    TotalTooLarge,
    #[error("path too deep: {0}")]
    PathTooDeep(String),
    #[error("path too long: {0}")]
    PathTooLong(String),
    #[error("unsafe path: {0}")]
    UnsafePath(String),
    #[error("unsupported entry kind: {0}")]
    UnsupportedEntryKind(String),
    #[error("malformed archive: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(String),
    #[error("sandbox trapped (fuel/memory exhausted): {0}")]
    SandboxTrapped(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractStats {
    pub entries: u64,
    pub bytes_written: u64,
}

/// Owns the engine + parsed component + a pre-instantiated linker.
/// Cheap to share across runtimes; per-call work in [`Self::extract`]
/// reuses the cached `InstancePre` so each extraction skips function
/// resolution.
pub struct WasmExtractor {
    engine: Engine,
    instance_pre: InstancePre<ExtractorState>,
}

impl WasmExtractor {
    pub fn new() -> Result<Self, ExtractError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config)
            .map_err(|e| ExtractError::Internal(format!("engine init: {e}")))?;
        let component = Component::new(&engine, EXTRACTOR_WASM)
            .map_err(|e| ExtractError::Internal(format!("parse extractor component: {e}")))?;
        let mut linker = Linker::<ExtractorState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync::<ExtractorState>(&mut linker)
            .map_err(|e| ExtractError::Internal(format!("link wasi: {e}")))?;
        let instance_pre = linker
            .instantiate_pre(&component)
            .map_err(|e| ExtractError::Internal(format!("instantiate_pre: {e}")))?;
        Ok(Self {
            engine,
            instance_pre,
        })
    }

    /// Run extraction on `blob_path` (a regular file), writing to
    /// `dest_dir` (which the caller must have created). Returns the
    /// component's reported stats; the host is expected to verify the
    /// reported `bytes_written` against an audit walk of `dest_dir`.
    pub fn extract(
        &self,
        format: ArchiveFormat,
        blob_path: &Path,
        dest_dir: &Path,
        strip_prefix: Option<&str>,
        limits: ExtractorLimits,
    ) -> Result<ExtractStats, ExtractError> {
        // Held to end of fn so the staged hardlink survives the call.
        let scratch = ExtractScratch::stage(blob_path)?;

        let wasi = WasiCtxBuilder::new()
            .preopened_dir(scratch.dir(), "/blob", DirPerms::READ, FilePerms::READ)
            .map_err(|e| ExtractError::Io(format!("preopen blob dir: {e}")))?
            .preopened_dir(dest_dir, "/out", DirPerms::all(), FilePerms::all())
            .map_err(|e| ExtractError::Io(format!("preopen out dir: {e}")))?
            .build();
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build();
        let mut store = Store::new(
            &self.engine,
            ExtractorState {
                wasi,
                table: ResourceTable::new(),
                limits: store_limits,
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| ExtractError::Internal(format!("set_fuel: {e}")))?;

        let instance = self
            .instance_pre
            .instantiate(&mut store)
            .map_err(|e| ExtractError::Internal(format!("instantiate: {e}")))?;
        let bindings = Extractor::new(&mut store, &instance)
            .map_err(|e| ExtractError::Internal(format!("bind extractor: {e}")))?;

        let options = wit_extract::ExtractOptions {
            format: format.into(),
            strip_prefix: strip_prefix.map(str::to_string),
            max_entries: limits.max_entries,
            max_file_size: limits.max_file_size,
            max_total_bytes: limits.max_total_bytes,
            max_path_depth: limits.max_path_depth,
            max_path_len: limits.max_path_len,
        };

        match bindings
            .omnifs_archive_extractor_extract()
            .call_extract(&mut store, &options)
        {
            Ok(Ok(stats)) => Ok(ExtractStats {
                entries: stats.entries,
                bytes_written: stats.bytes_written,
            }),
            Ok(Err(err)) => Err(translate_wit_error(err)),
            Err(trap) => Err(ExtractError::SandboxTrapped(format!("{trap:#}"))),
        }
    }
}

fn translate_wit_error(err: wit_extract::ExtractError) -> ExtractError {
    use wit_extract::ExtractError as W;
    match err {
        W::TooManyEntries => ExtractError::TooManyEntries,
        W::FileTooLarge(p) => ExtractError::FileTooLarge(p),
        W::TotalTooLarge => ExtractError::TotalTooLarge,
        W::PathTooDeep(p) => ExtractError::PathTooDeep(p),
        W::PathTooLong(p) => ExtractError::PathTooLong(p),
        W::UnsafePath(p) => ExtractError::UnsafePath(p),
        W::UnsupportedEntryKind(p) => ExtractError::UnsupportedEntryKind(p),
        W::Malformed(m) => ExtractError::Malformed(m),
        W::Io(m) => ExtractError::Io(m),
    }
}

struct ExtractorState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for ExtractorState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Per-extraction scratch directory. Hardlinks the source blob into
/// `<scratch>/blob.dat` so the WASI preopen exposes only the one file
/// the component needs to read. Cleaned up on drop.
struct ExtractScratch {
    dir: tempfile::TempDir,
}

impl ExtractScratch {
    fn stage(blob_path: &Path) -> Result<Self, ExtractError> {
        // Stage the scratch dir alongside the blob so the hardlink
        // never crosses devices (provider cache lives on one fs); /tmp
        // is tmpfs in containerized deployments and would force a copy.
        let parent = blob_path.parent().ok_or_else(|| {
            ExtractError::Io(format!("blob has no parent: {}", blob_path.display()))
        })?;
        let dir = tempfile::Builder::new()
            .prefix("omnifs-extractor-")
            .tempdir_in(parent)
            .map_err(|e| ExtractError::Io(format!("tempdir: {e}")))?;
        let target = dir.path().join("blob.dat");
        if let Err(e) = std::fs::hard_link(blob_path, &target) {
            if matches!(e.kind(), std::io::ErrorKind::CrossesDevices)
                || e.raw_os_error() == Some(libc::EXDEV)
            {
                std::fs::copy(blob_path, &target)
                    .map_err(|e2| ExtractError::Io(format!("copy blob to scratch: {e2}")))?;
            } else {
                return Err(ExtractError::Io(format!("hard_link blob to scratch: {e}")));
            }
        }
        Ok(Self { dir })
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// Walk `root` and sum the bytes of every regular file. Used by the
/// host to audit the component's reported `bytes_written` — defense in
/// depth, since the sandbox can't actually exceed the limit but a
/// future bug in the .wasm could under-report.
pub(crate) fn audit_bytes_written(root: &Path) -> Result<u64, std::io::Error> {
    fn visit(path: &Path, total: &mut u64) -> std::io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(path)? {
                visit(&entry?.path(), total)?;
            }
        } else if meta.is_file() {
            *total = total.saturating_add(meta.len());
        }
        Ok(())
    }
    let mut total = 0u64;
    visit(root, &mut total)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};

    fn shared_extractor() -> &'static Arc<WasmExtractor> {
        static EXTRACTOR: OnceLock<Arc<WasmExtractor>> = OnceLock::new();
        EXTRACTOR.get_or_init(|| Arc::new(WasmExtractor::new().expect("build extractor")))
    }

    use crate::runtime::test_archives::synthesize_targz;

    fn write_blob(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    fn run_extract(
        bytes: &[u8],
        format: ArchiveFormat,
        strip: Option<&str>,
        limits: ExtractorLimits,
    ) -> (PathBuf, Result<ExtractStats, ExtractError>) {
        let tmp = tempfile::tempdir().unwrap();
        let blob_path = tmp.path().join("blob");
        write_blob(&blob_path, bytes);
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let result = shared_extractor().extract(format, &blob_path, &dest, strip, limits);
        // Keep tmp alive for caller to inspect dest.
        std::mem::forget(tmp);
        (dest, result)
    }

    #[test]
    fn roundtrip_targz_extraction_writes_files_under_dest() {
        let bytes = synthesize_targz();
        let (dest, result) = run_extract(
            &bytes,
            ArchiveFormat::TarGz,
            Some("pkg-1.0/"),
            DEFAULT_LIMITS,
        );
        let stats = result.expect("extract ok");
        assert_eq!(stats.entries, 2);
        let cargo = std::fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
        let lib = std::fs::read_to_string(dest.join("src").join("lib.rs")).unwrap();
        assert!(lib.contains("answer"));
        let audited = audit_bytes_written(&dest).unwrap();
        assert_eq!(audited, stats.bytes_written);
    }

    #[test]
    fn limit_max_entries_trips_too_many_entries() {
        let bytes = synthesize_targz();
        let limits = ExtractorLimits {
            max_entries: 1,
            ..DEFAULT_LIMITS
        };
        let (_dest, result) = run_extract(&bytes, ArchiveFormat::TarGz, Some("pkg-1.0/"), limits);
        assert!(matches!(result, Err(ExtractError::TooManyEntries)));
    }

    #[test]
    fn limit_max_file_size_trips_file_too_large() {
        let bytes = synthesize_targz();
        let limits = ExtractorLimits {
            max_file_size: 4,
            ..DEFAULT_LIMITS
        };
        let (_dest, result) = run_extract(&bytes, ArchiveFormat::TarGz, Some("pkg-1.0/"), limits);
        assert!(matches!(result, Err(ExtractError::FileTooLarge(_))));
    }

    #[test]
    fn limit_max_total_bytes_trips_total_too_large() {
        let bytes = synthesize_targz();
        // Both files together are ~70 bytes; cap at 30 to trip the
        // running total mid-extract.
        let limits = ExtractorLimits {
            max_total_bytes: 30,
            ..DEFAULT_LIMITS
        };
        let (_dest, result) = run_extract(&bytes, ArchiveFormat::TarGz, Some("pkg-1.0/"), limits);
        assert!(matches!(
            result,
            Err(ExtractError::TotalTooLarge | ExtractError::FileTooLarge(_))
        ));
    }

    /// Hand-craft a single 512-byte tar header carrying a `../`-prefixed
    /// path so the extractor's `sanitize_path` is the thing under test
    /// (not `tar::Builder::set_path`, which rejects traversal upfront).
    fn raw_tar_with_path(path: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        // name (100 bytes)
        header[..path.len().min(100)].copy_from_slice(&path[..path.len().min(100)]);
        // mode "0000644 \0"
        header[100..108].copy_from_slice(b"0000644\0");
        // uid, gid: zeros padded
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size — octal, 11 chars + NUL
        let size_str = format!("{:011o}\0", payload.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        // mtime
        header[136..148].copy_from_slice(b"00000000000\0");
        // checksum field initially spaces
        header[148..156].fill(b' ');
        // typeflag: regular file '0'
        header[156] = b'0';
        // ustar magic
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // checksum: sum of all bytes treating cksum field as 8 spaces
        let cksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(cksum_str.as_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
        // pad payload to 512-byte block
        let pad = (512 - (payload.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        // two zero blocks signal end of archive
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    #[test]
    fn malicious_path_traversal_is_rejected() {
        let archive = raw_tar_with_path(b"../escape.txt", b"pwned");
        let (dest, result) = run_extract(&archive, ArchiveFormat::Tar, None, DEFAULT_LIMITS);
        match result {
            Err(ExtractError::UnsafePath(_) | ExtractError::Malformed(_)) => {},
            other => panic!("expected UnsafePath/Malformed, got {other:?}"),
        }
        // Defense in depth: even if path validation slipped, the WASI
        // preopen capability scope would block escape outside `/out`.
        assert!(!dest.parent().unwrap().join("escape.txt").exists());
    }
}
