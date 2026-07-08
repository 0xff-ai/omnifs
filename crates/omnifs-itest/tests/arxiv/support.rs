//! arXiv provider route-test helpers.

use omnifs_itest::RuntimeHarness;
use omnifs_wit::provider::types::{Effects, LogicalId};

pub use omnifs_itest::TestOpExt;

pub fn arxiv_harness() -> RuntimeHarness {
    RuntimeHarness::new(
        r#"
        {
            "provider": "omnifs_provider_arxiv.wasm",
            "mount": "arxiv",
            "capabilities": {
                "domains": ["export.arxiv.org", "arxiv.org"]
            }
        }
    "#,
    )
    .unwrap()
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

/// The canonical logical id of the first emitted canonical store, rendered as
/// `kind|name=value...`. Scenario snapshots render canonical view-leaf paths and
/// content shas but not the logical id, so identity assertions stay hand
/// written on this helper.
pub fn first_canonical_id(effects: &Effects) -> Option<String> {
    effects
        .canonical
        .first()
        .map(|store| canonical_id_string(&store.id))
}
