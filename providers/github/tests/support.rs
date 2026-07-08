//! GitHub provider route-test helpers.

use omnifs_itest::{RuntimeHarness, create_test_repo};
use omnifs_wit::provider::types::{ByteSource, Effects, FsKind, Stability};

pub use omnifs_itest::{TestOpExt, project_paths};

pub fn github_harness() -> RuntimeHarness {
    RuntimeHarness::new(
        r#"
        {
            "provider": "omnifs_provider_github.wasm",
            "mount": "github",
            "auth": {
                "type": "static-token",
                "scheme": "pat"
            },
            "capabilities": {
                "domains": ["api.github.com"],
                "git_repos": ["git@github.com:octocat/Hello-World.git"]
            }
        }
    "#,
    )
    .unwrap()
}

pub fn project_file_stability(effects: &Effects, path: &str) -> Option<Stability> {
    effects.fs.iter().find_map(|write| {
        if write.path != path {
            return None;
        }
        match &write.kind {
            FsKind::File(file) => Some(file.attrs.stability),
            FsKind::Directory(_) => None,
        }
    })
}

pub fn project_file_inline_bytes<'a>(effects: &'a Effects, path: &str) -> Option<&'a [u8]> {
    effects.fs.iter().find_map(|write| {
        if write.path != path {
            return None;
        }
        match &write.kind {
            FsKind::File(file) => match &file.bytes {
                ByteSource::Inline(bytes) => Some(bytes.as_slice()),
                _ => None,
            },
            FsKind::Directory(_) => None,
        }
    })
}

pub fn seed_github_repo_cache(harness: &RuntimeHarness, owner: &str, repo: &str) {
    let cache_path = harness
        .clone_dir
        .path()
        .join("github.com")
        .join(owner)
        .join(repo);
    create_test_repo(&cache_path, "Hello from cache\n");
    std::fs::write(
        cache_path.join(".omnifs-clone-url"),
        format!("git@github.com:{owner}/{repo}.git"),
    )
    .unwrap();
}
