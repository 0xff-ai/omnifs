#![cfg(not(target_os = "wasi"))]

mod scenarios;
mod support;

use omnifs_engine::test_support::TestOp;
use omnifs_itest::read_bytes;
use omnifs_wit::provider::types::{
    Callout, CalloutResult, Cursor, ErrorKind, HttpResponse, ListChildrenResult, LookupChildResult,
    OpResult, ReadFileOutcome, Stability,
};
use support::{TestOpExt, arxiv_harness, canonical_id_string, first_canonical_id};

const SAMPLE_PAPER_ATOM: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2604.00002v3</id>
    <updated>2026-04-02T00:00:00Z</updated>
    <published>2026-04-02T00:00:00Z</published>
    <title>Interleaved-DOI Paper</title>
    <summary>DOIs separated by    other elements.</summary>
    <author><name>Test Author</name></author>
    <arxiv:primary_category term="cs.AI"/>
    <arxiv:doi>10.48550/arXiv.2604.00002</arxiv:doi>
    <arxiv:journal_ref>Some Journal, 2026</arxiv:journal_ref>
    <arxiv:doi>10.1234/journal.2026.002</arxiv:doi>
  </entry>
</feed>"#;

const PAPER_ID: &str = "2604.00002";
const PAPER_ID_ANCHOR: &str = "arxiv.paper|paper=2604.00002";

fn resume_http(op: &mut TestOp<'_>, body: Vec<u8>) {
    op.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body,
    })])
    .unwrap();
}

fn resume_paper_atom(op: &mut TestOp<'_>) {
    resume_http(op, SAMPLE_PAPER_ATOM.to_vec());
}

fn resume_blob(op: &mut TestOp<'_>, blob: u64) {
    op.answer_callouts(vec![CalloutResult::BlobFetched(
        omnifs_wit::provider::types::BlobFetched {
            blob,
            size: 4,
            content_type: Some("application/pdf".to_string()),
            etag: None,
            status: 200,
            response_headers: Vec::new(),
        },
    )])
    .unwrap();
}

// TODO(tape): projection snapshots render canonical view-leaf paths and content
// shas, not the canonical logical id, so the identity collapse across the
// `/categories/{category}/papers` alias (both paths resolve to
// `arxiv.paper|paper=X`) is asserted here. The `attach_symmetry` and
// `paper_read` scenarios cover the shared canonical bytes across the two paths.
#[test]
fn attach_symmetry_collapses_paper_identity() {
    let harness = arxiv_harness();
    let direct_path = format!("/papers/{PAPER_ID}/@latest/paper.atom");
    let via_path = format!("/categories/cs.AI/papers/{PAPER_ID}/@latest/paper.atom");

    let mut direct = harness.read(&direct_path).unwrap();
    resume_paper_atom(&mut direct);

    let mut via_category = harness.read(&via_path).unwrap();
    resume_paper_atom(&mut via_category);

    let direct_id = first_canonical_id(direct.effects().unwrap()).expect("direct canonical id");
    let via_id =
        first_canonical_id(via_category.effects().unwrap()).expect("category canonical id");
    assert_eq!(direct_id, PAPER_ID_ANCHOR);
    assert_eq!(via_id, PAPER_ID_ANCHOR);
}

// TODO(tape): the `old-style-paper` scenario reads a real old-style id and its
// snapshot proves the encoded segment routes and fetches, but the canonical
// logical-id capture (`arxiv.paper|paper=cs.LG%2F0512345`, the percent-encoded
// slash) is not rendered, so the round-trip capture stays asserted here.
#[test]
fn old_style_encoded_id_round_trips() {
    let harness = arxiv_harness();
    let mut op = harness
        .read("/papers/cs.LG%2F0512345/@latest/paper.atom")
        .unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch.url.contains("id_list=cs.LG%2F0512345")
            || fetch.url.contains("id_list=cs.LG/0512345"),
        "unexpected fetch url: {}",
        fetch.url
    );
    resume_http(
        &mut op,
        br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/cs.LG/0512345v1</id><updated>2020-01-01</updated><published>2020-01-01</published><title>t</title><summary>s</summary></entry></feed>"#.to_vec(),
    );
    assert_eq!(
        first_canonical_id(op.effects().unwrap()).as_deref(),
        Some("arxiv.paper|paper=cs.LG%2F0512345")
    );
}

// TODO(tape): the blob url-pinning assertions (a `v1` request carries the
// version-pinned resource url, `@latest` does not) are not visible in a
// projection snapshot, so the `old-style-paper` scenario exercises the real
// `FetchBlob` tape arm and the stable-version stability branch, but the url
// construction and the `@latest`-mutable branch stay proven here on synthetic
// bytes.
#[test]
fn version_blob_immutable_latest_mutable() {
    let harness = arxiv_harness();
    let mut version_pdf_step = harness
        .read(&format!("/papers/{PAPER_ID}/v1/paper.pdf"))
        .unwrap();
    let atom_fetch = version_pdf_step.expect_single_fetch();
    assert!(atom_fetch.url.contains(PAPER_ID));
    resume_paper_atom(&mut version_pdf_step);
    let version_pdf = match version_pdf_step.callouts() {
        [Callout::FetchBlob(request)] => request,
        other => panic!("expected blob fetch callout, got {other:?}"),
    };
    assert!(version_pdf.url.contains("2604.00002v1"));
    resume_blob(&mut version_pdf_step, 1);
    match version_pdf_step.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Stable);
        },
        other => panic!("expected version pdf, got {other:?}"),
    }

    let mut latest_pdf_step = harness
        .read(&format!("/papers/{PAPER_ID}/@latest/paper.pdf"))
        .unwrap();
    let atom_fetch = latest_pdf_step.expect_single_fetch();
    assert!(atom_fetch.url.contains(PAPER_ID));
    resume_paper_atom(&mut latest_pdf_step);
    let latest_pdf = match latest_pdf_step.callouts() {
        [Callout::FetchBlob(request)] => request,
        other => panic!("expected blob fetch callout, got {other:?}"),
    };
    assert!(
        latest_pdf.url.contains("2604.00002.pdf"),
        "latest pdf url: {}",
        latest_pdf.url
    );
    assert!(
        !latest_pdf.url.contains("v1"),
        "latest pdf must not be version-pinned: {}",
        latest_pdf.url
    );
    resume_blob(&mut latest_pdf_step, 2);
    match latest_pdf_step.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_ne!(file.attrs.stability, Stability::Stable);
        },
        other => panic!("expected latest pdf, got {other:?}"),
    }
}

// TODO(tape): a real category recent-papers feed churns every re-record (it is
// the newest 50 ids), and the "no member canonicals" property is byte
// independent, so this invariant stays on a synthetic one-entry feed rather
// than a volatile recorded listing.
#[test]
fn category_listing_emits_no_member_canonicals() {
    let harness = arxiv_harness();
    let mut op = harness.list("/categories/cs.AI/papers").unwrap();
    let listed = op.expect_single_fetch();
    assert!(listed.url.contains("cs.AI"));
    resume_http(
        &mut op,
        format!(
            r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/{PAPER_ID}v1</id></entry></feed>"#
        )
        .into_bytes(),
    );
    assert!(op.effects().unwrap().canonical.is_empty());
    match op.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            assert_eq!(listing.entries.len(), 1);
            assert_eq!(listing.entries[0].name, PAPER_ID);
        },
        other => panic!("expected listing, got {other:?}"),
    }
}

// TODO(tape): the scenario `Step` API cannot drive a paging cursor and the
// projection snapshot does not render `next_cursor`, so category pagination
// (page 0 carries a next cursor; page 1 requests `start=50`) stays here on a
// synthetic feed.
#[test]
fn category_listing_paginates() {
    let harness = arxiv_harness();
    let mut page0 = harness.list("/categories/cs.AI/papers").unwrap();
    let page0_fetch = page0.expect_single_fetch();
    assert!(page0_fetch.url.contains("start=0"));
    resume_http(
        &mut page0,
        (0..50)
            .map(|i| {
                format!(
                    r"<entry><id>http://arxiv.org/abs/2604.{i:05}v1</id></entry>",
                    i = i + 1
                )
            })
            .fold(
                br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">"#.to_vec(),
                |mut acc, entry| {
                    acc.extend_from_slice(entry.as_bytes());
                    acc
                },
            )
            .into_iter()
            .chain(br"</feed>".iter().copied())
            .collect(),
    );
    match page0.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            assert_eq!(listing.entries.len(), 50);
            assert!(matches!(listing.next_cursor, Some(Cursor::Page(1))));
        },
        other => panic!("expected paged listing, got {other:?}"),
    }

    let page1 = harness
        .list_with_cursor("/categories/cs.AI/papers", Some(Cursor::Page(1)))
        .unwrap();
    let page1_fetch = page1.expect_single_fetch();
    assert!(page1_fetch.url.contains("start=50"));
}

/// Adversarial cases: routing rejections and upstream-error mapping that no
/// happy-path scenario exercises. These stay hand written on synthetic bytes
/// because they assert error outcomes and request shapes, not recorded content.
mod adversarial {
    use super::*;

    #[test]
    fn versioned_paper_segment_rejected() {
        let harness = arxiv_harness();
        let read = harness
            .read("/papers/2401.12345v2/@latest/paper.json")
            .unwrap();
        match read.result().unwrap() {
            OpResult::Error(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected versioned id read to fail, got {other:?}"),
        }
        let lookup = harness.lookup("/papers", "2401.12345v2").unwrap();
        match lookup.result().unwrap() {
            OpResult::LookupChild(LookupChildResult::NotFound(_)) => {},
            other => panic!("expected lookup miss, got {other:?}"),
        }
    }

    #[test]
    fn missing_paper_emits_not_found_with_id() {
        let harness = arxiv_harness();
        let mut op = harness
            .read("/papers/99999999.99999/@latest/paper.atom")
            .unwrap();
        let fetch = op.expect_single_fetch();
        assert!(fetch.url.contains("99999999.99999"));
        resume_http(
            &mut op,
            br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"></feed>"#.to_vec(),
        );
        match op.result().unwrap() {
            OpResult::ReadFile(ReadFileOutcome::NotFound(Some(id))) => {
                assert_eq!(canonical_id_string(id), "arxiv.paper|paper=99999999.99999");
            },
            other => panic!("expected not-found with id, got {other:?}"),
        }
    }

    #[test]
    fn representation_dispatch_respects_declared_leaves() {
        let harness = arxiv_harness();
        let mut json = harness
            .read(&format!("/papers/{PAPER_ID}/@latest/paper.json"))
            .unwrap();
        resume_paper_atom(&mut json);
        assert!(matches!(
            json.result().unwrap(),
            OpResult::ReadFile(ReadFileOutcome::Found(_))
        ));
        let mut raw = harness
            .read(&format!("/papers/{PAPER_ID}/@latest/paper.atom"))
            .unwrap();
        resume_paper_atom(&mut raw);
        let raw_bytes = read_bytes(&raw);
        assert_eq!(raw_bytes, SAMPLE_PAPER_ATOM);

        let md = harness
            .read(&format!("/papers/{PAPER_ID}/@latest/paper.md"))
            .unwrap();
        match md.result().unwrap() {
            OpResult::Error(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected undeclared paper.md to be missing, got {other:?}"),
        }

        let direct_latest = harness
            .read(&format!("/papers/{PAPER_ID}/paper.json"))
            .unwrap();
        match direct_latest.result().unwrap() {
            OpResult::Error(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected old direct paper.json to be missing, got {other:?}"),
        }

        let old_versions = harness
            .read(&format!("/papers/{PAPER_ID}/versions/v1/paper.pdf"))
            .unwrap();
        match old_versions.result().unwrap() {
            OpResult::Error(error) => assert_eq!(error.kind, ErrorKind::NotFound),
            other => panic!("expected old versions path to be missing, got {other:?}"),
        }
    }
}
