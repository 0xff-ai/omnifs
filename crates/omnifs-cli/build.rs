use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];
const ARCHIVE_TOOL_WASM: &str = "omnifs_tool_archive.wasm";
const PROVIDER_BUNDLE_ARCHIVE: &str = "provider-bundle.tar.zst";
const PROVIDER_BUNDLE_DIR_ENV: &str = "OMNIFS_PROVIDER_BUNDLE_DIR";

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.join("../..");
    let provider_root = manifest_dir.join("../../providers");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dev_mounts_dir = out_dir.join("dev-mounts");

    println!("cargo:rerun-if-changed={}", provider_root.display());
    println!("cargo:rerun-if-env-changed={PROVIDER_BUNDLE_DIR_ENV}");

    let mut manifest_paths = fs::read_dir(&provider_root)
        .unwrap_or_else(|error| panic!("read {}: {error}", provider_root.display()))
        .filter_map(|entry| {
            let entry =
                entry.unwrap_or_else(|error| panic!("scan {}: {error}", provider_root.display()));
            let manifest = entry.path().join("omnifs.provider.json");
            let provider_name = entry.file_name();
            let provider_name = provider_name
                .to_str()
                .unwrap_or_else(|| panic!("invalid provider dir {}", entry.path().display()));
            (manifest.is_file() && !FIXTURE_PROVIDER_DIRS.contains(&provider_name))
                .then_some(manifest)
        })
        .collect::<Vec<_>>();
    manifest_paths.sort();

    let mut dev_mount_out =
        String::from("pub(crate) static EMBEDDED_DEV_MOUNTS: &[(&str, &str)] = &[\n");
    let mut provider_files = Vec::new();

    let _ = fs::remove_dir_all(&dev_mounts_dir);
    fs::create_dir_all(&dev_mounts_dir).expect("create embedded dev-mounts dir");

    for manifest_path in manifest_paths {
        let provider_dir = manifest_path
            .parent()
            .expect("provider manifest path has a parent");
        let provider_name = provider_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("invalid provider dir {}", provider_dir.display()));

        println!("cargo:rerun-if-changed={}", manifest_path.display());
        let manifest_json = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
        let manifest_value: serde_json::Value = serde_json::from_str(&manifest_json)
            .unwrap_or_else(|error| panic!("parse {}: {error}", manifest_path.display()));
        let provider_file = manifest_value
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("{} must set provider", manifest_path.display()));
        provider_files.push(provider_file.to_string());

        // A provider is auto-mounted by `omnifs dev` only if it ships a
        // `dev-mount.json`. Providers that need external setup before they can
        // mount (e.g. kubernetes needs a live cluster) keep their mount spec
        // under `testenv/` and are mounted by their own flow instead.
        let dev_mount_path = provider_dir.join("dev-mount.json");
        if !dev_mount_path.is_file() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", dev_mount_path.display());
        let dev_json = fs::read_to_string(&dev_mount_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", dev_mount_path.display()));

        let dev_value: serde_json::Value = serde_json::from_str(&dev_json)
            .unwrap_or_else(|error| panic!("parse dev mount for `{provider_name}`: {error}"));
        let mount_name = dev_value
            .get("mount")
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("dev mount for `{provider_name}` must set `mount`"));
        let filename = format!("{mount_name}.json");
        fs::write(
            dev_mounts_dir.join(&filename),
            ensure_trailing_newline(&dev_json),
        )
        .unwrap_or_else(|error| panic!("write embedded dev mount `{filename}`: {error}"));

        writeln!(
            dev_mount_out,
            "    (\"{filename}\", include_str!(\"dev-mounts/{filename}\")),"
        )
        .unwrap();
    }

    dev_mount_out.push_str("];\n");

    fs::write(out_dir.join("embedded_dev_mounts.rs"), dev_mount_out)
        .expect("write embedded dev mount list");

    write_provider_bundle(&workspace_root, &out_dir, provider_files);
}

fn ensure_trailing_newline(json: &str) -> String {
    if json.ends_with('\n') {
        json.to_string()
    } else {
        format!("{json}\n")
    }
}

fn write_provider_bundle(
    workspace_root: &std::path::Path,
    out_dir: &std::path::Path,
    mut files: Vec<String>,
) {
    files.push(ARCHIVE_TOOL_WASM.to_string());
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
                "read provider bundle artifact {}: {error}\nrun `just providers-build` first, or set {PROVIDER_BUNDLE_DIR_ENV} to a directory containing the built provider WASM artifacts",
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
        "provider bundle artifacts are missing; searched:\n{searched}\nrun `just providers-build` first, or set {PROVIDER_BUNDLE_DIR_ENV} to the built WASM directory"
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
