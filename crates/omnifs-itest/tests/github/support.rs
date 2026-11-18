//! GitHub provider route-test helpers.

use std::future::Future;
use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_engine::{LookupAnswer, Namespace};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{
    ByteSource, Callout, CalloutResult, Effects, FsKind, HttpRequest, HttpResponse, Stability,
};

pub use omnifs_itest::{TestOpExt, project_paths};

pub async fn resolve_namespace(namespace: &dyn Namespace, path: &str) -> LookupAnswer {
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
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

/// Run a namespace operation, answer the fetches it makes in order with
/// `answers`, and return its output with those requests. An operation that
/// makes another number of fetches times out.
pub async fn answering<T>(
    harness: &RuntimeHarness,
    operation: impl Future<Output = T>,
    answers: Vec<HttpResponse>,
) -> (T, Vec<HttpRequest>) {
    let runtime = &harness.runtime;
    let answer = async {
        let mut requests = Vec::new();
        for response in answers {
            let callout = loop {
                match runtime.try_recv_test_callout() {
                    Some(callout) => break callout,
                    None => tokio::time::sleep(Duration::from_millis(2)).await,
                }
            };
            let Callout::Fetch(request) = callout.callout() else {
                panic!("expected a fetch, got {:?}", callout.callout());
            };
            requests.push(request.clone());
            callout.answer(CalloutResult::HttpResponse(response));
        }
        requests
    };
    let (output, requests) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(operation, answer)
    })
    .await
    .expect("the operation makes exactly the answered fetches");
    (output, requests)
}

/// The value of header `name` of `request`.
pub fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}
