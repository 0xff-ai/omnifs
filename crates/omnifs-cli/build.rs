use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let provider_root = manifest_dir.join("../../providers");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dev_mounts_dir = out_dir.join("dev-mounts");

    println!("cargo:rerun-if-changed={}", provider_root.display());

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

    let mut manifest_out = String::from("&[\n");
    let mut dev_mount_out =
        String::from("pub(crate) static EMBEDDED_DEV_MOUNTS: &[(&str, &str)] = &[\n");

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

        writeln!(
            manifest_out,
            "    include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../providers/{provider_name}/omnifs.provider.json\")),"
        )
        .unwrap();

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

    manifest_out.push_str("]\n");
    dev_mount_out.push_str("];\n");

    fs::write(out_dir.join("builtin_provider_manifests.rs"), manifest_out)
        .expect("write built-in provider manifest list");
    fs::write(out_dir.join("embedded_dev_mounts.rs"), dev_mount_out)
        .expect("write embedded dev mount list");
}

fn ensure_trailing_newline(json: &str) -> String {
    if json.ends_with('\n') {
        json.to_string()
    } else {
        format!("{json}\n")
    }
}
