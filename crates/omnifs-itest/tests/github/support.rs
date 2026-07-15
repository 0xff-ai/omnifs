//! GitHub provider route-test helpers.

use omnifs_core::path::Path;
use omnifs_engine::{LookupAnswer, Namespace};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::provider::types::{ByteSource, Effects, FsKind, Stability};

pub use omnifs_itest::{TestOpExt, project_paths};

pub async fn resolve_namespace(namespace: &dyn Namespace, path: &str) -> LookupAnswer {
    let mut answer = LookupAnswer {
        path: Path::root(),
        attrs: namespace.getattr(Path::root()).await.unwrap(),
    };
    for segment in Path::parse(path).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

pub fn github_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_github.wasm",
            "mount": "github",
            "auth": {
                "type": "static-token",
                "scheme": "pat"
            }
        }
    "#,
    )
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
