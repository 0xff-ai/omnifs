use omnifs_engine::{Namespace, TreeNamespace};
use omnifs_nfs::{Export, ReadOnlyExport};
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::provider::{Artifact, ProviderStore};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::runtime::Runtime;

#[path = "support/registry.rs"]
mod registry_support;

#[test]
#[ignore = "requires built provider WASM artifacts, network access, gh auth, Docker, and optional LINEAR_TOKEN"]
fn real_provider_export_smoke() {
    let harness = RealProviders::new();
    let export = &harness.export;

    assert_root_shape(export);
    smoke_dns(export);
    smoke_db(export);
    smoke_docker(export);
    smoke_github(export);
    smoke_arxiv(export);
    smoke_linear(export, harness.linear_enabled);
}

fn assert_root_shape(export: &Export) {
    let root_listing = export.readdir(export.root()).expect("root listing");
    let root_names = root_listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    for mount in ["arxiv", "db", "dns", "docker", "github"] {
        assert!(
            root_names.contains(mount),
            "missing {mount} in root listing"
        );
    }
    assert!(
        !root_names.contains("omnifs"),
        "NFS export must not synthesize a top-level omnifs directory"
    );
}

fn smoke_dns(export: &Export) {
    let dns_a = read_path(export, &["dns", "example.com", "A"]);
    assert!(
        String::from_utf8_lossy(&dns_a).contains('.'),
        "DNS A response should contain at least one address"
    );
    let dns_mx = read_path(export, &["dns", "@google", "example.com", "MX"]);
    assert!(
        !dns_mx.is_empty(),
        "explicit resolver MX response should not be empty"
    );
}

fn smoke_db(export: &Export) {
    let info = read_path(export, &["db", "meta", "info.json"]);
    let info_json = serde_json::from_slice::<serde_json::Value>(&info).expect("db info JSON");
    assert_eq!(info_json["path"], "/data/test.db");
    assert_eq!(
        String::from_utf8(read_path(export, &["db", "meta", "path.txt"])).expect("db path utf8"),
        "/data/test.db\n"
    );
    assert_eq!(
        String::from_utf8(read_path(export, &["db", "tables", "Album", "count.txt"]))
            .expect("db count utf8"),
        "2\n"
    );
    let sample = read_path(export, &["db", "tables", "Album", "sample.json"]);
    assert!(
        String::from_utf8_lossy(&sample).contains("For Those About To Rock"),
        "db sample should include fixture row"
    );
}

fn smoke_docker(export: &Export) {
    assert_eq!(
        String::from_utf8(read_path(export, &["docker", "system", "ping"]))
            .expect("docker ping utf8"),
        "OK\n"
    );
    let docker_version = read_path(export, &["docker", "system", "version.json"]);
    assert!(
        String::from_utf8_lossy(&docker_version).contains("Version"),
        "docker version JSON should include version data"
    );
    let containers = read_path(export, &["docker", "containers.json"]);
    serde_json::from_slice::<serde_json::Value>(&containers).expect("containers.json is JSON");
}

fn smoke_github(export: &Export) {
    let github_root = lookup_path(export, &["github", "0xff-ai", "omnifs"]);
    let github_children = export
        .readdir(github_root)
        .expect("github repo listing")
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<BTreeSet<_>>();
    for child in ["actions", "issues", "pulls", "repo"] {
        assert!(
            github_children.contains(child),
            "github repo listing missing {child}"
        );
    }

    let readme = read_path(
        export,
        &["github", "0xff-ai", "omnifs", "repo", "README.md"],
    );
    assert!(
        String::from_utf8_lossy(&readme).contains("omnifs"),
        "github TreeRef README should be readable"
    );
}

fn smoke_arxiv(export: &Export) {
    let paper = read_path(
        export,
        &["arxiv", "papers", "1706.03762", "v7", "paper.json"],
    );
    let paper_json = serde_json::from_slice::<serde_json::Value>(&paper).expect("arxiv JSON");
    assert_eq!(paper_json["raw_arxiv_id"], "1706.03762");
    assert_eq!(paper_json["current_version"], "v7");
}

fn smoke_linear(export: &Export, linear_enabled: bool) {
    if linear_enabled {
        let teams = lookup_path(export, &["linear", "teams"]);
        let listing = export.readdir(teams).expect("linear teams listing");
        assert!(!listing.entries.is_empty(), "linear should list teams");
    } else {
        eprintln!("LINEAR_TOKEN not set; skipped live Linear provider read");
    }
}

struct RealProviders {
    export: Arc<Export>,
    linear_enabled: bool,
    _runtime: Runtime,
    _config_dir: TempDir,
    _cache_dir: TempDir,
    _clone_dir: TempDir,
    _db_dir: TempDir,
}

impl RealProviders {
    fn new() -> Self {
        let config_dir = tempfile::tempdir().expect("config dir");
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let clone_dir = tempfile::tempdir().expect("clone dir");
        let db_dir = tempfile::tempdir().expect("db dir");
        let mounts_dir = config_dir.path().join("mounts");
        let providers_dir = config_dir.path().join("providers");
        std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
        std::fs::create_dir_all(&providers_dir).expect("providers dir");

        let linear_enabled = std::env::var_os("LINEAR_TOKEN").is_some();
        copy_real_providers(&providers_dir, linear_enabled);
        write_real_mounts(
            &mounts_dir,
            config_dir.path(),
            db_dir.path(),
            linear_enabled,
        );

        let runtime = Runtime::new().expect("tokio runtime");
        let registry = Arc::new(registry_support::load_registry_from_mount_dir(
            cache_dir.path(),
            config_dir.path(),
            &providers_dir,
            &config_dir.path().join("credentials.json"),
            clone_dir.path(),
            &mounts_dir,
            runtime.handle(),
        ));
        let namespace = TreeNamespace::online(Arc::clone(&registry), runtime.handle().clone());
        let export = Arc::new(Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        ));
        Self {
            export,
            linear_enabled,
            _runtime: runtime,
            _config_dir: config_dir,
            _cache_dir: cache_dir,
            _clone_dir: clone_dir,
            _db_dir: db_dir,
        }
    }
}

fn copy_real_providers(providers_dir: &Path, linear_enabled: bool) {
    for wasm in [
        "omnifs_provider_arxiv.wasm",
        "omnifs_provider_db.wasm",
        "omnifs_provider_dns.wasm",
        "omnifs_provider_docker.wasm",
        "omnifs_provider_github.wasm",
    ] {
        copy_provider(providers_dir, wasm);
    }
    if linear_enabled {
        copy_provider(providers_dir, "omnifs_provider_linear.wasm");
    }
}

fn write_real_mounts(mounts_dir: &Path, config_dir: &Path, db_dir: &Path, linear_enabled: bool) {
    write_credentials(config_dir, linear_enabled);
    create_sqlite_fixture(&db_dir.join("test.db"));

    write_mount(
        mounts_dir,
        "arxiv",
        r#"{"provider":"omnifs_provider_arxiv.wasm","mount":"arxiv"}"#,
    );
    write_mount(
        mounts_dir,
        "dns",
        r#"{"provider":"omnifs_provider_dns.wasm","mount":"dns"}"#,
    );
    write_mount(mounts_dir, "docker", DOCKER_MOUNT_JSON);
    write_mount(mounts_dir, "github", GITHUB_MOUNT_JSON);
    write_mount(mounts_dir, "db", &db_mount_json(db_dir));
    if linear_enabled {
        write_mount(
            mounts_dir,
            "linear",
            r#"{"provider":"omnifs_provider_linear.wasm","mount":"linear","auth":{"type":"static-token","scheme":"pat"}}"#,
        );
    }
}

/// Populate the credential store the registry reads. Tokens now live only in
/// the store; the mount spec's auth carries identity (scheme), never a source.
/// Entries mirror `omnifs_workspace::creds::CredentialEntry`'s wire form.
fn write_credentials(config_dir: &Path, linear_enabled: bool) {
    let mut entries = vec![format!(
        r#""github:pat:default":{}"#,
        static_token_entry(&gh_token())
    )];
    if linear_enabled {
        let token = std::env::var("LINEAR_TOKEN").expect("LINEAR_TOKEN");
        entries.push(format!(
            r#""linear:pat:default":{}"#,
            static_token_entry(&token)
        ));
    }
    let path = config_dir.join("credentials.json");
    let body = entries.join(",");
    std::fs::write(&path, format!(r#"{{"version":1,"entries":{{{body}}}}}"#))
        .expect("write credentials.json");
    set_private_file_mode(&path);
}

fn static_token_entry(token: &str) -> String {
    format!(
        r#"{{"kind":"static-token","access_token":{token:?},"refresh_token":null,"expires_at":null,"token_type":"Bearer","stored_at":"1970-01-01T00:00:00Z","last_validated":null,"scopes":[],"upstream_identity":null,"extras":{{}}}}"#
    )
}

const GITHUB_MOUNT_JSON: &str = r#"{
    "provider":"omnifs_provider_github.wasm",
    "mount":"github",
    "auth":{"type":"static-token","scheme":"pat"}
}"#;

const DOCKER_MOUNT_JSON: &str = r#"{
    "provider":"omnifs_provider_docker.wasm",
    "mount":"docker",
    "limits":{"max_memory_mb":64},
    "config":{"endpoint":"unix:///var/run/docker.sock"}
}"#;

fn db_mount_json(_db_dir: &Path) -> String {
    format!(
        r#"{{
            "provider":"omnifs_provider_db.wasm",
            "mount":"db",
            "limits":{{"max_memory_mb":128}},
            "config":{{
                "path":"/data/test.db",
                "read_only":true,
                "sample_limit":20
            }}
        }}"#
    )
}

fn lookup_path(export: &Export, path: &[&str]) -> u64 {
    path.iter().fold(export.root(), |parent, name| {
        export
            .lookup(parent, name)
            .unwrap_or_else(|status| panic!("lookup {name:?} under {parent}: {status:?}"))
    })
}

fn read_path(export: &Export, path: &[&str]) -> Vec<u8> {
    let id = lookup_path(export, path);
    export
        .read(id)
        .unwrap_or_else(|status| panic!("read {path:?}: {status:?}"))
}

fn copy_provider(providers_dir: &Path, wasm: &str) {
    let artifact = Artifact::from_file(provider_wasm_path(wasm))
        .unwrap_or_else(|error| panic!("parse provider {wasm}: {error}"));
    let store = ProviderStore::new(providers_dir);
    store
        .retain(&artifact)
        .unwrap_or_else(|error| panic!("index provider {wasm}: {error}"));
}

/// The pinned reference for a built provider wasm, named from its file stem.
fn provider_reference(wasm: &str) -> ProviderRef {
    Artifact::from_file(provider_wasm_path(wasm))
        .unwrap_or_else(|error| panic!("parse provider {wasm}: {error}"))
        .reference()
}

fn provider_wasm_path(plugin_name: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root");
    let path = workspace_root
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(plugin_name);
    assert!(
        path.exists(),
        "{plugin_name} not found at {path}. Run `just build providers` first.",
        path = path.display()
    );
    path
}

fn write_mount(mounts_dir: &Path, name: &str, json: &str) {
    // Rewrite the `provider` filename in the spec body to the pinned reference
    // for the installed artifact.
    let mut value: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|error| panic!("parse mount {name}: {error}"));
    let wasm = value["provider"]
        .as_str()
        .unwrap_or_else(|| panic!("mount {name} has no provider filename"))
        .to_string();
    value["provider"] = serde_json::to_value(provider_reference(&wasm))
        .unwrap_or_else(|error| panic!("serialize provider ref for {name}: {error}"));
    std::fs::write(
        mounts_dir.join(format!("{name}.json")),
        serde_json::to_string(&value).unwrap(),
    )
    .unwrap_or_else(|error| panic!("write mount {name}: {error}"));
}

fn gh_token() -> String {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .expect("run gh auth token");
    assert!(
        output.status.success(),
        "gh auth token failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("gh token is utf8")
}

fn create_sqlite_fixture(path: &Path) {
    let status = Command::new("sqlite3")
        .arg(path)
        .arg(
            "CREATE TABLE Album (AlbumId INTEGER PRIMARY KEY, Title TEXT NOT NULL); \
             INSERT INTO Album VALUES (1, 'For Those About To Rock'), (2, 'Balls to the Wall'); \
             CREATE INDEX idx_album_title ON Album(Title);",
        )
        .status()
        .expect("run sqlite3");
    assert!(status.success(), "sqlite3 fixture creation failed");
}

#[cfg(unix)]
fn set_private_file_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .expect("token metadata")
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions).expect("token file mode");
}

#[cfg(not(unix))]
fn set_private_file_mode(_path: &Path) {}
