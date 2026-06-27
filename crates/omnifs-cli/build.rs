use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];
const PROVIDER_BUNDLE_ARCHIVE: &str = "provider-bundle.tar.zst";
const PROVIDER_BUNDLE_DIR_ENV: &str = "OMNIFS_PROVIDER_BUNDLE_DIR";

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.join("../..");
    let provider_root = manifest_dir.join("../../providers");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", provider_root.display());
    println!("cargo:rerun-if-env-changed={PROVIDER_BUNDLE_DIR_ENV}");

    // Each provider crate `providers/<name>` (crate `omnifs-provider-<name>`)
    // builds to `omnifs_provider_<name>.wasm`. The provider manifest now travels
    // inside that wasm as the `omnifs.provider-metadata.v1` custom section
    // (authored from `#[provider]` annotations), so there is no
    // `omnifs.provider.json` to read here: the set of providers is the set of
    // crate dirs under `providers/`, minus the test fixtures.
    let mut provider_files = Vec::new();
    let read = fs::read_dir(&provider_root)
        .unwrap_or_else(|error| panic!("read {}: {error}", provider_root.display()));
    for entry in read {
        let entry =
            entry.unwrap_or_else(|error| panic!("scan {}: {error}", provider_root.display()));
        let dir_name = entry.file_name();
        let Some(dir_name) = dir_name.to_str() else {
            continue;
        };
        if FIXTURE_PROVIDER_DIRS.contains(&dir_name) || !entry.path().join("Cargo.toml").is_file() {
            continue;
        }
        provider_files.push(format!(
            "omnifs_provider_{}.wasm",
            dir_name.replace('-', "_")
        ));
    }
    provider_files.sort();

    write_provider_bundle(&workspace_root, &out_dir, provider_files);
}

fn write_provider_bundle(
    workspace_root: &std::path::Path,
    out_dir: &std::path::Path,
    mut files: Vec<String>,
) {
    files.sort();
    files.dedup();

    let artifact_dir = find_artifact_dir(workspace_root, &files);
    let archive_path = out_dir.join(PROVIDER_BUNDLE_ARCHIVE);
    let writer = Vec::new();
    let encoder =
        zstd::stream::write::Encoder::new(writer, 19).expect("create provider bundle zstd encoder");
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);

    for file in files {
        let path = artifact_dir.join(&file);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "read provider bundle artifact {}: {error}\nrun `just providers build` first, or set {PROVIDER_BUNDLE_DIR_ENV} to a directory containing the built provider WASM artifacts",
                path.display()
            )
        });
        assert!(
            !bytes.is_empty(),
            "provider bundle artifact {} is empty",
            path.display()
        );

        let mut header = tar::Header::new_gnu();
        header.set_size(
            bytes
                .len()
                .try_into()
                .expect("provider artifact fits in u64"),
        );
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        archive
            .append_data(&mut header, file, bytes.as_slice())
            .expect("append provider artifact to embedded bundle");
    }

    let encoder = archive.into_inner().expect("finish provider bundle tar");
    let compressed = encoder
        .finish()
        .expect("finish provider bundle zstd stream");
    let mut output = fs::File::create(&archive_path)
        .unwrap_or_else(|error| panic!("create {}: {error}", archive_path.display()));
    output
        .write_all(&compressed)
        .unwrap_or_else(|error| panic!("write {}: {error}", archive_path.display()));
}

fn find_artifact_dir(workspace_root: &std::path::Path, files: &[String]) -> PathBuf {
    for dir in artifact_dirs(workspace_root) {
        if files.iter().all(|file| dir.join(file).is_file()) {
            return dir;
        }
    }

    let searched = artifact_dirs(workspace_root)
        .into_iter()
        .map(|path| format!("  {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    panic!(
        "provider bundle artifacts are missing; searched:\n{searched}\nrun `just providers build` first, or set {PROVIDER_BUNDLE_DIR_ENV} to the built WASM directory"
    );
}

fn artifact_dirs(workspace_root: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = env::var_os(PROVIDER_BUNDLE_DIR_ENV) {
        dirs.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(path);
        let target_dir = if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        };
        dirs.push(target_dir.join("wasm32-wasip2/release"));
    }
    dirs.push(workspace_root.join("target/wasm32-wasip2/release"));
    dirs.dedup();
    dirs
}
