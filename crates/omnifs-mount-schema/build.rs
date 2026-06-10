use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let provider_root = manifest_dir.join("../../providers");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

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
    }
    manifest_out.push_str("]\n");

    fs::write(out_dir.join("builtin_provider_manifests.rs"), manifest_out)
        .expect("write built-in provider manifest list");
}
