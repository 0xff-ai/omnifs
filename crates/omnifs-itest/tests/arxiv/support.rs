//! arXiv provider route-test helpers.

use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::provider::types::{Callout, Effects, LogicalId};

pub use omnifs_itest::TestOpExt;

pub fn arxiv_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_arxiv.wasm",
            "mount": "arxiv"
        }
    "#,
    )
}

pub fn canonical_id_string(id: &LogicalId) -> String {
    let mut out = id.kind.clone();
    for cap in &id.captures {
        out.push('|');
        out.push_str(&cap.name);
        out.push('=');
        out.push_str(&cap.value);
    }
    out
}

pub fn first_canonical_id(effects: &Effects) -> Option<String> {
    effects
        .canonical
        .first()
        .map(|store| canonical_id_string(&store.id))
}

pub fn count_fetch_callouts<T>(ops: &[&omnifs_engine::test_support::TestOp<'_, T>]) -> usize {
    ops.iter()
        .flat_map(|op| op.callouts().iter())
        .filter(|callout| matches!(callout, Callout::Fetch(_)))
        .count()
}
