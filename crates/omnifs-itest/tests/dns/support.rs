//! DNS provider route-test helpers.

use omnifs_itest::{RuntimeHarness, TestOpExt, make_initialized_runtime};
use omnifs_wit::provider::types::{Callout, HttpRequest};
use std::net::Ipv4Addr;
use std::str::FromStr;

pub fn dns_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_dns.wasm",
            "mount": "dns",
            "capabilities": {
                "domains": ["cloudflare-dns.com", "dns.google"]
            }
        }
    "#,
    )
}

pub fn expect_fetches(op: &omnifs_engine::test_support::TestOp<'_>) -> Vec<HttpRequest> {
    op.callouts()
        .iter()
        .map(|callout| match callout {
            Callout::Fetch(request) => request.clone(),
            other => panic!("expected fetch callout, got {other:?}"),
        })
        .collect()
}

pub fn expect_fetch(op: &omnifs_engine::test_support::TestOp<'_>) -> HttpRequest {
    op.expect_single_fetch().clone()
}

pub fn canned_a_response(domain: &str, ip: &str) -> Vec<u8> {
    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::rdata::A;
    use hickory_proto::rr::{Name, RData, Record, RecordType};

    let name = Name::from_ascii(format!("{domain}.")).unwrap();
    let mut msg = Message::new(0, MessageType::Response, OpCode::Query);
    msg.add_query(Query::query(name.clone(), RecordType::A));
    msg.add_answer(Record::from_rdata(
        name,
        300,
        RData::A(A(Ipv4Addr::from_str(ip).unwrap())),
    ));
    msg.to_vec().unwrap()
}
