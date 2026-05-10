//! Sandboxed archive extraction adapter for the embedded Wasm tool.
//!
//! The host ships a precompiled `omnifs-tool-archive.wasm` (built
//! from `crates/omnifs-tool-archive`) and runs it in a fresh
//! `wasmtime::Store` for each `open-archive` callout. The component
//! sees only:
//!
//! - `/blob/blob.dat`: read-only preopen of the archive bytes.
//! - `/out/`: read-write preopen of the destination directory.
//!
//! plus an engine-enforced fuel and memory cap. The generic Wasmtime
//! and WASI mechanics live in the host-internal `runtime::wasm` and
//! `runtime::sandbox` modules; this module owns only the archive
//! extractor's WIT adapter, options, stats, and domain errors.

use crate::extractor_bindings::Extractor;
use crate::extractor_bindings::exports::omnifs::tool_archive::extract as wit_extract;
use crate::runtime::sandbox::preopen::StagedBlob;
use crate::runtime::wasm;
use std::path::Path;
use wasmtime::component::{Component, InstancePre, Linker, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Embedded extractor wasm artifact. The path resolves relative to
/// `crates/host/src/`. `just build-providers` (and the `Extractor`
/// Docker build step) ensure this file exists before the host crate
/// compiles.
const EXTRACTOR_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/omnifs_tool_archive.wasm"
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

/// Resource and shape limits enforced for one archive extraction.
#[derive(Clone, Copy, Debug)]
pub struct ExtractorLimits {
    /// Maximum number of archive entries the component may write.
    pub max_entries: u64,
    /// Maximum size of any single extracted file.
    pub max_file_size: u64,
    /// Maximum total bytes written across all extracted files.
    pub max_total_bytes: u64,
    /// Maximum path component depth after optional prefix stripping.
    pub max_path_depth: u32,
    /// Maximum rendered path length after optional prefix stripping.
    pub max_path_len: u32,
    /// Wasmtime fuel budget for the extraction call.
    pub fuel: u64,
    /// Maximum linear memory allocated by the extractor component.
    pub max_memory_bytes: usize,
}

/// Archive formats accepted by the host extractor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive.
    TarGz,
    /// Uncompressed tar archive.
    Tar,
    /// Zip archive.
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

impl ArchiveFormat {
    pub(crate) const fn cache_component(self) -> &'static str {
        match self {
            Self::TarGz => "targz",
            Self::Tar => "tar",
            Self::Zip => "zip",
        }
    }
}

/// Failure returned by the sandboxed extractor or its host wrapper.
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

/// Extraction counters reported by the component.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractStats {
    /// Number of archive entries written to the destination tree.
    pub entries: u64,
    /// Total file bytes written to the destination tree.
    pub bytes_written: u64,
}

/// Owns the engine, parsed component, and pre-instantiated linker for
/// the archive extractor tool.
///
/// Cheap to share across runtimes; per-call work in [`Self::extract`]
/// reuses the cached `InstancePre` so each extraction skips function
/// resolution.
pub struct ArchiveExtractorComponent {
    engine: Engine,
    instance_pre: InstancePre<ExtractorState>,
    limits: ExtractorLimits,
}

impl ArchiveExtractorComponent {
    /// Compile and pre-instantiate the embedded extractor component.
    pub fn new(limits: ExtractorLimits) -> Result<Self, ExtractError> {
        let engine = wasm::component_engine(|config| {
            config.consume_fuel(true);
        })
        .map_err(|e| ExtractError::Internal(format!("engine init: {e}")))?;
        let component = Component::new(&engine, EXTRACTOR_WASM)
            .map_err(|e| ExtractError::Internal(format!("parse extractor component: {e}")))?;
        let mut linker = Linker::<ExtractorState>::new(&engine);
        wasm::add_wasi_to_linker::<ExtractorState>(&mut linker)
            .map_err(|e| ExtractError::Internal(format!("link wasi: {e}")))?;
        let instance_pre = linker
            .instantiate_pre(&component)
            .map_err(|e| ExtractError::Internal(format!("instantiate_pre: {e}")))?;
        Ok(Self {
            engine,
            instance_pre,
            limits,
        })
    }

    /// Run extraction on `blob_path` (a regular file), writing to
    /// `dest_dir` (which the caller must have created). Returns the
    /// component's reported stats.
    pub fn extract(
        &self,
        format: ArchiveFormat,
        blob_path: &Path,
        dest_dir: &Path,
        strip_prefix: Option<&str>,
    ) -> Result<ExtractStats, ExtractError> {
        let limits = self.limits;
        // Held to end of fn so the staged hardlink survives the call.
        let scratch = StagedBlob::stage(blob_path).map_err(|e| ExtractError::Io(e.to_string()))?;

        let wasi = WasiCtxBuilder::new()
            .preopened_dir(scratch.dir(), "/blob", DirPerms::READ, FilePerms::READ)
            .map_err(|e| ExtractError::Io(format!("preopen blob dir: {e}")))?
            .preopened_dir(dest_dir, "/out", DirPerms::all(), FilePerms::all())
            .map_err(|e| ExtractError::Io(format!("preopen out dir: {e}")))?
            .build();
        let store_limits = wasm::store_limits(limits.max_memory_bytes);
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
            .omnifs_tool_archive_extract()
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::path::PathBuf;

    const CARGO_TOML_BYTES: &[u8] = b"[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n";
    const LIB_RS_BYTES: &[u8] = b"pub fn answer() -> u32 { 42 }\n";

    fn synthesize_targz() -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append_targz_file(&mut tar, "pkg-1.0/Cargo.toml", CARGO_TOML_BYTES);
            append_targz_file(&mut tar, "pkg-1.0/src/lib.rs", LIB_RS_BYTES);
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    fn append_targz_file(
        tar: &mut tar::Builder<&mut GzEncoder<Vec<u8>>>,
        path: &str,
        bytes: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, bytes).unwrap();
    }

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
        let extractor = ArchiveExtractorComponent::new(limits).expect("build extractor");
        let result = extractor.extract(format, &blob_path, &dest, strip);
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
        assert_eq!(
            stats.bytes_written,
            (CARGO_TOML_BYTES.len() + LIB_RS_BYTES.len()) as u64
        );
        let cargo = std::fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"pkg\""));
        let lib = std::fs::read_to_string(dest.join("src").join("lib.rs")).unwrap();
        assert!(lib.contains("answer"));
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
        // size: octal, 11 chars + NUL
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
