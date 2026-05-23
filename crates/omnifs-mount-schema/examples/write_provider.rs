fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = omnifs_mount_schema::provider_manifest_json();
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}
