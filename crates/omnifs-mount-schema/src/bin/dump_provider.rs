//! Regenerate `schema/omnifs.provider.schema.json` from the derived schemars model.

fn main() {
    let schema = omnifs_mount_schema::provider_manifest_json();
    let value = serde_json::to_value(schema).expect("schema serializes");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/omnifs.provider.schema.json");
    let text = serde_json::to_string_pretty(&value).expect("pretty json");
    std::fs::write(&path, format!("{text}\n")).expect("write schema file");
    eprintln!("wrote {}", path.display());
}
