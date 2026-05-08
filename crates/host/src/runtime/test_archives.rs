//! Shared test fixtures for the archive/extractor lib tests.
//!
//! Lives next to the consumers so both `runtime::archive::tests` and
//! `runtime::wasm_extractor::tests` can build the same canonical
//! `.tar.gz` rather than each carry its own copy.

use flate2::Compression;
use flate2::write::GzEncoder;

/// Build an in-memory `.tar.gz` containing two files under `pkg-1.0/`:
/// a `Cargo.toml` and `src/lib.rs`. Used by the extractor tests to
/// exercise the gzip + tar walk and the strip-prefix logic.
pub fn synthesize_targz() -> Vec<u8> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let cargo_toml = b"[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/Cargo.toml").unwrap();
        header.set_size(cargo_toml.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &cargo_toml[..]).unwrap();

        let lib_rs = b"pub fn answer() -> u32 { 42 }\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg-1.0/src/lib.rs").unwrap();
        header.set_size(lib_rs.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &lib_rs[..]).unwrap();

        tar.finish().unwrap();
    }
    gz.finish().unwrap()
}
