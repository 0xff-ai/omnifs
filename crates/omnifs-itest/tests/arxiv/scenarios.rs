//! Data-driven arXiv scenarios over the callout tape system.
//!
//! Each scenario records real arXiv HTTP callouts once (via
//! `just host itest-record arxiv <scenario>`) and replays them hermetically in
//! the default host-test lane. arXiv is a public API, so scenarios declare no
//! auth and record verbatim bodies: the raw Atom formatting quirks (namespaces,
//! whitespace, interleaved elements) are exactly what the paper parser must
//! survive, so keeping the bytes byte-faithful is the point.
//!
//! Papers are chosen to be old and stable so the recorded feeds and PDF do not
//! drift: `0704.0001` (the first new-scheme id, 2007) exercises the common
//! new-style projection, and `hep-th/9711200` (Maldacena 1997) is an old-style
//! id with a slash whose small PDF keeps the blob sidecar durable and cheap.

use omnifs_itest::scenario::{Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

/// The arXiv mount config the scenarios record against: the arXiv-owned domains
/// for the metadata (`export.arxiv.org`) and resource (`arxiv.org`) callouts,
/// and no auth (the API is public).
const ARXIV_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_arxiv.wasm",
    "mount": "arxiv",
    "capabilities": {
        "domains": ["export.arxiv.org", "arxiv.org"]
    }
}
"#;

/// A new-style paper's core projection: the raw canonical Atom, the derived
/// metadata JSON at `@latest` and at a numbered version, and the version-family
/// directory listing. The first step is a cold canonical read, so its snapshot
/// pins the paper's canonical logical id (`arxiv.paper[paper=0704.0001]`); the
/// `attach_symmetry` scenario reaches the same id through the category alias.
#[test]
fn paper_read() {
    run(&Scenario {
        name: "paper-read",
        dir: "arxiv",
        config: ARXIV_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/papers/0704.0001/@latest/paper.atom"),
            Step::Read("/papers/0704.0001/@latest/paper.json"),
            Step::Read("/papers/0704.0001/v1/paper.json"),
            Step::List("/papers/0704.0001"),
        ],
    });
}

/// Attach symmetry: reading a paper through the `/categories/{category}/papers`
/// alias resolves to the same object identity as the direct `/papers` path. The
/// category segment is not part of the paper key, so this cold read pins the
/// same canonical id as `paper_read`'s first step, proving the alias collapses
/// onto one identity.
#[test]
fn attach_symmetry() {
    run(&Scenario {
        name: "attach-symmetry",
        dir: "arxiv",
        config: ARXIV_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[Step::Read(
            "/categories/hep-ph/papers/0704.0001/@latest/paper.atom",
        )],
    });
}

/// An old-style id (with an archive prefix and slash) round-trips through the
/// percent-encoded path segment, and a numbered-version PDF read exercises the
/// `FetchBlob` tape arm and its `tapes/blobs/` sidecar. The cold Atom read pins
/// the encoded canonical id (`arxiv.paper[paper=hep-th%2F9711200]`); the PDF read
/// then fetches the immutable `v1` blob, so the recorded bytes stay durable.
#[test]
fn old_style_paper() {
    run(&Scenario {
        name: "old-style-paper",
        dir: "arxiv",
        config: ARXIV_CONFIG,
        auth: None,
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/papers/hep-th%2F9711200/@latest/paper.atom"),
            Step::Read("/papers/hep-th%2F9711200/v1/paper.pdf"),
        ],
    });
}
