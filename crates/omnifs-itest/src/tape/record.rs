//! Tape recorder: the `CalloutObserver` tee that turns live callout exchanges
//! into a scrubbed, on-disk tape.
//!
//! The recorder is installed as the engine's observer during a record-mode
//! scenario run. It buffers the verbatim `wit` callout/result pairs, then
//! [`TapeRecorder::finalize`] scrubs them (credentials removed, headers dropped,
//! bodies policy-applied) and writes the tape. A post-write tripwire re-reads
//! the file and decodes every written body (including base64 and sidecar tiers)
//! and panics if any injected credential survived, so a scrubbing bug fails the
//! recording loudly instead of leaking to disk.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use omnifs_engine::test_support::CalloutObserver;
use omnifs_wit::provider::types as wit;

use super::scrub::{
    RecordedResponse, RewrittenResponse, ScrubError, TapeRules, rewrite_response, scrub_request,
};
use super::{
    Tape, TapeBody, TapeEntry, TapeError, TapeHeader, TapeKind, TapeRequest, TapeResponse,
};

/// Collects (callout, result) pairs during a record-mode scenario run.
/// Installed as the engine's [`CalloutObserver`]; drained by [`Self::finalize`].
pub struct TapeRecorder {
    entries: Mutex<Vec<RecordedExchange>>,
}

/// One captured exchange, verbatim and pre-scrub. It never leaves this module
/// unscrubbed: [`TapeRecorder::finalize`] is the only reader.
struct RecordedExchange {
    kind: TapeKind,
    callout: wit::Callout,
    result: wit::CalloutResult,
}

impl CalloutObserver for TapeRecorder {
    fn observe(&self, _op_id: u64, callout: &wit::Callout, result: &wit::CalloutResult) {
        // Only Fetch/FetchBlob are taped (invariant I1 scope); git, archive, and
        // read-blob run through real local executors in both record and replay.
        let kind = match callout {
            wit::Callout::Fetch(_) => TapeKind::HttpFetch,
            wit::Callout::FetchBlob(_) => TapeKind::BlobFetch,
            _ => return,
        };
        // Short critical section shared with the engine driver thread.
        self.entries
            .lock()
            .expect("recorder mutex poisoned")
            .push(RecordedExchange {
                kind,
                callout: callout.clone(),
                result: result.clone(),
            });
    }
}

impl TapeRecorder {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(Vec::new()),
        })
    }

    /// Convert recorded exchanges into scrubbed tape entries and write the tape.
    ///
    /// `blob_bytes` resolves a `FetchBlob` result's bytes from the harness blob
    /// cache (via `Runtime::blob_cache_for_tests` + `BlobCache::bytes_for_tests`)
    /// because `BlobFetched` carries only metadata. `secrets` is the credential
    /// material injected for this recording; the post-write tripwire scans for
    /// it.
    ///
    /// # Panics
    ///
    /// Panics if any string in `secrets` survives into the written tape. The
    /// tripwire is defense in depth for invariant I2 (credentials never touch
    /// disk in a tape), not a recoverable error path.
    pub fn finalize(
        self: Arc<Self>,
        rules: &TapeRules,
        tape_path: &Path,
        sidecar_dir: &Path,
        blob_bytes: impl Fn(u64) -> Option<Vec<u8>>,
        secrets: &[String],
    ) -> Result<(), TapeError> {
        let exchanges = std::mem::take(&mut *self.entries.lock().expect("recorder mutex poisoned"));
        let mut entries = Vec::with_capacity(exchanges.len());
        for (index, exchange) in exchanges.into_iter().enumerate() {
            let request = tape_request(&exchange, sidecar_dir)?;
            let response = tape_response(rules, &exchange, &blob_bytes, sidecar_dir)?;
            entries.push(TapeEntry {
                seq: u32::try_from(index).expect("recording exceeded u32 entries"),
                kind: exchange.kind,
                request,
                response,
            });
        }
        Tape::save(tape_path, &entries)?;
        assert_no_secrets(tape_path, sidecar_dir, &entries, secrets);
        Ok(())
    }
}

/// Scrub the request side of an exchange into a persistable [`TapeRequest`].
/// Method, url, and headers come from [`scrub_request`]; the body is encoded
/// separately and verbatim (only its digest participates in matching).
fn tape_request(exchange: &RecordedExchange, sidecar_dir: &Path) -> Result<TapeRequest, TapeError> {
    let (method, url, headers, body, cache_key) = match &exchange.callout {
        wit::Callout::Fetch(req) => (
            req.method.as_str(),
            req.url.as_str(),
            req.headers.as_slice(),
            req.body.as_deref(),
            None,
        ),
        wit::Callout::FetchBlob(req) => (
            req.method.as_str(),
            req.url.as_str(),
            req.headers.as_slice(),
            req.body.as_deref(),
            Some(req.cache_key.clone()),
        ),
        // observe() only buffers Fetch/FetchBlob, so this is unreachable in
        // practice; surface it as a corrupt-recording error rather than panic.
        _ => return Err(corrupt("recorded a non-fetch callout")),
    };
    let scrubbed = scrub_request(method, url, headers, None);
    let body = match body {
        Some(bytes) if !bytes.is_empty() => Some(TapeBody::encode(bytes, sidecar_dir)?),
        _ => None,
    };
    Ok(TapeRequest {
        method: scrubbed.method,
        url: scrubbed.url,
        headers: scrubbed.headers,
        body,
        cache_key,
    })
}

/// Rewrite the response side of an exchange (drop headers, apply the body
/// policy) and encode its body into a [`TapeResponse`]. For a `FetchBlob`
/// result the body bytes are resolved through `blob_bytes` because the wit
/// `BlobFetched` carries only metadata.
fn tape_response(
    rules: &TapeRules,
    exchange: &RecordedExchange,
    blob_bytes: &impl Fn(u64) -> Option<Vec<u8>>,
    sidecar_dir: &Path,
) -> Result<TapeResponse, TapeError> {
    let recorded = match &exchange.result {
        wit::CalloutResult::HttpResponse(r) => RecordedResponse::Http {
            status: r.status,
            headers: tape_headers(&r.headers),
            body: r.body.clone(),
        },
        wit::CalloutResult::BlobFetched(r) => {
            let bytes = blob_bytes(r.blob).ok_or_else(|| {
                corrupt(format!("blob {} bytes missing during recording", r.blob))
            })?;
            RecordedResponse::Blob {
                status: r.status,
                content_type: r.content_type.clone(),
                etag: r.etag.clone(),
                response_headers: tape_headers(&r.response_headers),
                body: bytes,
            }
        },
        wit::CalloutResult::CalloutError(e) => RecordedResponse::Error {
            // The kind is the Debug rendering of the wit ErrorKind, e.g.
            // "Network"; the replayer parses it back on load.
            kind: format!("{:?}", e.kind),
            message: e.message.clone(),
            retryable: e.retryable,
        },
        // A Fetch/FetchBlob callout never yields these result arms.
        _ => return Err(corrupt("recorded a non-fetch callout result")),
    };
    let response = match rewrite_response(rules, recorded).map_err(scrub_to_tape)? {
        RewrittenResponse::Http {
            status,
            headers,
            body,
        } => TapeResponse::Http {
            status,
            headers,
            body: TapeBody::encode(&body, sidecar_dir)?,
        },
        RewrittenResponse::Blob {
            status,
            content_type,
            etag,
            response_headers,
            body,
        } => TapeResponse::Blob {
            status,
            content_type,
            etag,
            response_headers,
            body: TapeBody::encode(&body, sidecar_dir)?,
        },
        RewrittenResponse::Error {
            kind,
            message,
            retryable,
        } => TapeResponse::Error {
            kind,
            message,
            retryable,
        },
    };
    Ok(response)
}

fn tape_headers(headers: &[wit::Header]) -> Vec<TapeHeader> {
    headers
        .iter()
        .map(|h| TapeHeader {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect()
}

/// Post-write tripwire: panic if any credential value survived into anything
/// this recording wrote. Two passes cover every byte on disk: a plain per-line
/// substring scan of the tape file (urls, headers, error messages, inline text
/// bodies), and a decoded-body scan of every request and response body, because
/// a secret inside a `Base64` body never appears as a plain substring in the
/// jsonl and a `Sidecar` body lives in a separate file entirely.
fn assert_no_secrets(
    tape_path: &Path,
    sidecar_dir: &Path,
    entries: &[TapeEntry],
    secrets: &[String],
) {
    let secrets: Vec<&String> = secrets.iter().filter(|s| !s.is_empty()).collect();
    if secrets.is_empty() {
        return;
    }
    let content = std::fs::read_to_string(tape_path).expect("tripwire re-read of written tape");
    for line in content.lines() {
        for secret in &secrets {
            assert!(
                !line.contains(secret.as_str()),
                "tape tripwire: credential material survived scrubbing into {}",
                tape_path.display()
            );
        }
    }
    for entry in entries {
        let response_body = match &entry.response {
            TapeResponse::Http { body, .. } | TapeResponse::Blob { body, .. } => Some(body),
            TapeResponse::Error { .. } => None,
        };
        for body in entry.request.body.iter().chain(response_body) {
            let bytes = body
                .decode(sidecar_dir)
                .expect("tripwire decode of just-written body");
            for secret in &secrets {
                assert!(
                    !contains_bytes(&bytes, secret.as_bytes()),
                    "tape tripwire: credential material survived scrubbing into {}",
                    tape_path.display()
                );
            }
        }
    }
}

/// Byte-window search for `needle` in `haystack`. `needle` is never empty here
/// (empty secrets are filtered before the scan).
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn corrupt(message: impl Into<String>) -> TapeError {
    TapeError::Io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

fn scrub_to_tape(err: ScrubError) -> TapeError {
    TapeError::Io(io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::scrub::BodyPolicy;

    fn header(name: &str, value: &str) -> wit::Header {
        wit::Header {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    fn fetch(url: &str, headers: Vec<wit::Header>) -> wit::Callout {
        wit::Callout::Fetch(wit::HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers,
            body: None,
        })
    }

    fn http_ok(body: &[u8]) -> wit::CalloutResult {
        wit::CalloutResult::HttpResponse(wit::HttpResponse {
            status: 200,
            headers: vec![header("content-type", "application/json")],
            body: body.to_vec(),
        })
    }

    #[test]
    fn scrubs_sensitive_header_and_query_before_disk() {
        let recorder = TapeRecorder::new();
        recorder.observe(
            0,
            &fetch(
                "https://api.example.com/data?access_token=topsecretvalue&page=1",
                vec![
                    header("Accept", "application/json"),
                    header("Authorization", "Bearer topsecretheader"),
                ],
            ),
            &http_ok(b"{\"ok\":true}"),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let tape_path = dir.path().join("scrub.jsonl");
        let sidecar_dir = dir.path().join("blobs");
        recorder
            .finalize(
                &TapeRules::default(),
                &tape_path,
                &sidecar_dir,
                |_| None,
                &["topsecretvalue".to_owned(), "topsecretheader".to_owned()],
            )
            .expect("finalize");

        let written = std::fs::read_to_string(&tape_path).expect("read tape");
        assert!(
            written.contains("<scrubbed>"),
            "expected a scrubbed sentinel"
        );
        assert!(!written.contains("topsecretvalue"));
        assert!(!written.contains("topsecretheader"));
        // The scrubbed param name and structure survive; only the value is gone.
        assert!(written.contains("access_token=<scrubbed>"));
    }

    #[test]
    #[should_panic(expected = "tape tripwire")]
    fn tripwire_panics_when_a_secret_survives_in_a_verbatim_body() {
        let recorder = TapeRecorder::new();
        // The secret rides in the response body, which Verbatim keeps byte for
        // byte, so scrubbing cannot remove it and the tripwire must fire.
        recorder.observe(
            0,
            &fetch("https://api.example.com/echo", vec![]),
            &http_ok(b"{\"leaked\":\"survivingsecret\"}"),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let tape_path = dir.path().join("leak.jsonl");
        let sidecar_dir = dir.path().join("blobs");
        // Panics inside finalize after the write; the Result is never observed.
        let _ = recorder.finalize(
            &TapeRules {
                drop_response_headers: &[],
                body: BodyPolicy::Verbatim,
            },
            &tape_path,
            &sidecar_dir,
            |_| None,
            &["survivingsecret".to_owned()],
        );
    }

    /// Drive one exchange with `body` through finalize with `secret` armed.
    /// Panics (the tripwire) or returns normally.
    fn finalize_with_body(body: &[u8], secret: &str) {
        let recorder = TapeRecorder::new();
        recorder.observe(
            0,
            &fetch("https://api.example.com/echo", vec![]),
            &http_ok(body),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = recorder.finalize(
            &TapeRules::default(),
            &dir.path().join("leak.jsonl"),
            &dir.path().join("blobs"),
            |_| None,
            &[secret.to_owned()],
        );
    }

    #[test]
    #[should_panic(expected = "tape tripwire")]
    fn tripwire_panics_when_a_secret_hides_in_a_base64_body() {
        // Non-UTF-8 padding forces the base64 tier, where the secret's bytes
        // never appear as a plain substring in the jsonl.
        let mut body = vec![0xff, 0xfe];
        body.extend_from_slice(b"base64hiddensecret");
        body.push(0xff);
        finalize_with_body(&body, "base64hiddensecret");
    }

    #[test]
    #[should_panic(expected = "tape tripwire")]
    fn tripwire_panics_when_a_secret_hides_in_a_sidecar_body() {
        // A UTF-8 body over the 256 KiB text ceiling spills to a sidecar file
        // the jsonl only references by hash; the tripwire must decode it.
        let mut body = vec![b'x'; crate::tape::TEXT_MAX + 1];
        let secret = b"sidecarhiddensecret";
        body[1000..1000 + secret.len()].copy_from_slice(secret);
        finalize_with_body(&body, "sidecarhiddensecret");
    }
}
