use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];
const PROVIDER_BUNDLE_ARCHIVE: &str = "provider-bundle.tar.zst";
const PROVIDER_BUNDLE_DIR_ENV: &str = "OMNIFS_PROVIDER_BUNDLE_DIR";
const PROVIDER_INDEX_VERSION: u32 = 2;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.join("../..");
    let provider_root = manifest_dir.join("../../providers");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed={}", provider_root.display());
    println!("cargo:rerun-if-env-changed={PROVIDER_BUNDLE_DIR_ENV}");

    let mut files = Vec::new();
    let read = fs::read_dir(&provider_root)
        .unwrap_or_else(|error| panic!("read {}: {error}", provider_root.display()));
    for entry in read {
        let entry = entry.unwrap_or_else(|error| panic!("scan providers: {error}"));
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if FIXTURE_PROVIDER_DIRS.contains(&name) || !entry.path().join("Cargo.toml").is_file() {
            continue;
        }
        files.push(format!("omnifs_provider_{}.wasm", name.replace('-', "_")));
    }
    files.sort();
    files.dedup();
    let (dir, files) = artifact_source(&workspace_root, &files);
    let bundle = out_dir.join(PROVIDER_BUNDLE_ARCHIVE);
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 19)
        .expect("create provider bundle zstd encoder");
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);
    for file in files {
        let path = dir.join(&file);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!("read provider bundle artifact {}: {error}", path.display())
        });
        assert!(
            !bytes.is_empty(),
            "provider artifact {} is empty",
            path.display()
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        archive
            .append_data(&mut header, file, bytes.as_slice())
            .expect("append provider artifact");
    }
    let encoder = archive.into_inner().expect("finish provider bundle tar");
    let compressed = encoder.finish().expect("finish provider bundle zstd");
    let mut output = fs::File::create(&bundle).expect("create provider bundle archive");
    output
        .write_all(&compressed)
        .expect("write provider bundle archive");
}

/// Mirrors the fields `build.rs` needs from `omnifs_provider::store::Index`.
/// Kept local rather than pulled in as a build-dependency: `omnifs-provider`
/// carries a large graph for a build script that only needs the index version
/// plus two fields per entry. Unknown entry fields are ignored; the schema's
/// authority is `ProviderStore::read_index`, and this is its narrow build-time
/// subset.
#[derive(serde::Deserialize)]
struct StoreIndex {
    version: u32,
    providers: Vec<StoreIndexEntry>,
}

#[derive(serde::Deserialize)]
struct StoreIndexEntry {
    id: String,
    name: String,
}

/// Select exactly one artifact per non-fixture provider name from the
/// store's index, rather than globbing the directory: a stray valid `.wasm`
/// that was never retained into the index must not enter the daemon binary.
fn select_from_index(index_path: &std::path::Path) -> Vec<String> {
    let bytes = fs::read(index_path).unwrap_or_else(|error| {
        panic!(
            "read provider store index {}: {error}",
            index_path.display()
        )
    });
    let index: StoreIndex = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "parse provider store index {}: {error}",
            index_path.display()
        )
    });
    assert_eq!(
        index.version,
        PROVIDER_INDEX_VERSION,
        "provider store index at {} has unsupported version {}",
        index_path.display(),
        index.version,
    );

    let mut by_name: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut ids = std::collections::BTreeSet::new();
    for entry in index.providers {
        assert!(
            ids.insert(entry.id.clone()),
            "provider store index at {} contains duplicate id {}",
            index_path.display(),
            entry.id,
        );
        if FIXTURE_PROVIDER_DIRS.contains(&entry.name.as_str()) {
            continue;
        }
        by_name
            .entry(entry.name)
            .or_default()
            .push(format!("{}.wasm", entry.id));
    }

    by_name
        .into_iter()
        .map(|(name, mut artifacts)| {
            artifacts.sort();
            artifacts.dedup();
            assert!(
                artifacts.len() == 1,
                "provider store index at {} retains {} distinct artifacts for `{name}`, expected exactly one: {artifacts:?}",
                index_path.display(),
                artifacts.len(),
            );
            artifacts.remove(0)
        })
        .collect()
}

fn artifact_source(workspace_root: &std::path::Path, files: &[String]) -> (PathBuf, Vec<String>) {
    let mut dirs = Vec::new();
    if let Some(path) = env::var_os(PROVIDER_BUNDLE_DIR_ENV) {
        dirs.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        };
        dirs.push(path.join("wasm32-wasip2/release"));
    }
    dirs.push(workspace_root.join("target/wasm32-wasip2/release"));
    dirs.dedup();
    for dir in &dirs {
        let index_path = dir.join("index.json");
        if index_path.is_file() {
            let selected = select_from_index(&index_path);
            if !selected.is_empty() {
                return (dir.clone(), selected);
            }
        }
        if files.iter().all(|file| dir.join(file).is_file()) {
            return (dir.clone(), files.to_vec());
        }
    }
    let searched = dirs
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    panic!(
        "provider bundle artifacts are missing; searched:\n{searched}\nrun `just build providers` first, or set {PROVIDER_BUNDLE_DIR_ENV}"
    );
}
