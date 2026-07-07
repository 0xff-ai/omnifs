//! Tape replayer: answers captured callouts from a loaded tape.
//!
//! Each callout is reduced to a [`MatchKey`] (scrubbed method/url, request-body
//! digest, and the conditional request headers that change response semantics),
//! and entries with the same key are answered FIFO. A `FetchBlob` answer
//! rematerializes the recorded bytes into the runtime's blob cache and points a
//! fresh `BlobFetched` at them, so downstream `read-blob`/`open-archive` behave
//! exactly as after a real fetch. A miss renders an actionable report naming the
//! nearest recorded candidates and the re-record command.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use omnifs_engine::test_support::blob::BlobCache;
use omnifs_wit::provider::types as wit;

use super::scrub::scrub_request;
use super::{
    Tape, TapeBody, TapeEntry, TapeError, TapeHeader, TapeKind, TapeRequest, TapeResponse,
    sha256_hex,
};

/// Answers captured callouts from a loaded tape.
pub struct TapePlayer {
    /// Remaining entries, FIFO per match key. Popping an entry consumes it.
    queues: HashMap<MatchKey, VecDeque<TapeEntry>>,
    /// `tapes/blobs`, the sidecar directory for out-of-line bodies.
    sidecar_dir: PathBuf,
    /// Kept for the miss report only: the tape's own path, an inventory of
    /// every loaded entry (so consumed entries still appear as candidates), and
    /// the set of consumed seqs.
    tape_path: PathBuf,
    catalog: Vec<Candidate>,
    consumed: HashSet<u32>,
}

/// The discriminants that decide which recorded exchange answers a callout.
/// Auth headers are scrubbed away; accept/user-agent are provider constants
/// that would make tapes brittle for no discrimination gain, so only the
/// conditional headers participate beyond method/url/body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MatchKey {
    kind: TapeKind,
    method: String,
    /// Scrubbed url, so record and replay compare scrubbed-to-scrubbed.
    url: String,
    /// `SHA-256` of the request body, when present.
    body_sha256: Option<String>,
    /// `if-none-match` / `if-modified-since`, lowercased name, sorted by name.
    conditional: Vec<(String, String)>,
}

/// A loaded entry reduced to what the miss report needs.
struct Candidate {
    seq: u32,
    kind: TapeKind,
    method: String,
    url: String,
}

/// Request header names that change response semantics and therefore
/// participate in matching. Compared case-insensitively.
const CONDITIONAL_HEADERS: [&str; 2] = ["if-none-match", "if-modified-since"];

impl TapePlayer {
    /// Load a tape and index its entries by match key. The sidecar directory is
    /// the tape's parent joined with `blobs`.
    pub fn load(tape_path: &Path) -> Result<Self, TapeError> {
        let sidecar_dir = tape_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("blobs");
        let tape = Tape::load(tape_path)?;

        let catalog = tape
            .entries
            .iter()
            .map(|entry| Candidate {
                seq: entry.seq,
                kind: entry.kind,
                method: entry.request.method.clone(),
                url: entry.request.url.clone(),
            })
            .collect();

        let mut queues: HashMap<MatchKey, VecDeque<TapeEntry>> = HashMap::new();
        for entry in tape.entries {
            let key = match_key_for_request(entry.kind, &entry.request, &sidecar_dir)?;
            queues.entry(key).or_default().push_back(entry);
        }

        Ok(Self {
            queues,
            sidecar_dir,
            tape_path: tape_path.to_path_buf(),
            catalog,
            consumed: HashSet::new(),
        })
    }

    /// Answer one pending callout. For a `Blob` response, materialize the
    /// recorded bytes into `blobs` and build a `BlobFetched` pointing at the
    /// freshly minted id.
    pub fn answer(
        &mut self,
        callout: &wit::Callout,
        blobs: &BlobCache,
    ) -> Result<wit::CalloutResult, TapeError> {
        let key = match_key_for_callout(callout)
            .ok_or_else(|| corrupt("replay received a non-fetch callout"))?;
        let Some(entry) = self.queues.get_mut(&key).and_then(VecDeque::pop_front) else {
            return Err(TapeError::Unmatched {
                rendered: self.render_miss(&key),
            });
        };
        self.consumed.insert(entry.seq);
        self.build_result(entry, blobs)
    }

    /// Entries never consumed. Informational, not an error: a scenario may
    /// exercise a subset of a shared tape.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }

    fn build_result(
        &self,
        entry: TapeEntry,
        blobs: &BlobCache,
    ) -> Result<wit::CalloutResult, TapeError> {
        match entry.response {
            TapeResponse::Http {
                status,
                headers,
                body,
            } => Ok(wit::CalloutResult::HttpResponse(wit::HttpResponse {
                status,
                headers: wit_headers(headers),
                body: decode_body(&body, &self.sidecar_dir)?,
            })),
            TapeResponse::Blob {
                status,
                content_type,
                etag,
                response_headers,
                body,
            } => {
                let bytes = decode_body(&body, &self.sidecar_dir)?;
                // The recorded cache key reproduces the real fetch's keying; a
                // fresh blob id is minted on every insert.
                let cache_key = entry
                    .request
                    .cache_key
                    .unwrap_or_else(|| format!("tape-blob-{}", entry.seq));
                let header_pairs = response_headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect();
                let id = blobs.insert_for_tests(
                    &cache_key,
                    &bytes,
                    content_type.clone(),
                    etag.clone(),
                    status,
                    header_pairs,
                );
                Ok(wit::CalloutResult::BlobFetched(wit::BlobFetched {
                    blob: id,
                    size: bytes.len() as u64,
                    content_type,
                    etag,
                    status,
                    response_headers: wit_headers(response_headers),
                }))
            },
            TapeResponse::Error {
                kind,
                message,
                retryable,
            } => Ok(wit::CalloutResult::CalloutError(wit::CalloutError {
                kind: parse_error_kind(&kind)?,
                message,
                retryable,
            })),
        }
    }

    fn render_miss(&self, pending: &MatchKey) -> String {
        render_miss_report(
            &self.tape_path,
            pending.kind,
            &pending.method,
            &pending.url,
            &self.catalog,
            &self.consumed,
        )
    }
}

/// Reduce a live callout to its [`MatchKey`], scrubbing first so it compares
/// against the scrubbed tape. Returns `None` for non-fetch callouts, which
/// replay never answers.
fn match_key_for_callout(callout: &wit::Callout) -> Option<MatchKey> {
    let (kind, method, url, headers, body) = match callout {
        wit::Callout::Fetch(req) => (
            TapeKind::HttpFetch,
            req.method.as_str(),
            req.url.as_str(),
            req.headers.as_slice(),
            req.body.as_deref(),
        ),
        wit::Callout::FetchBlob(req) => (
            TapeKind::BlobFetch,
            req.method.as_str(),
            req.url.as_str(),
            req.headers.as_slice(),
            req.body.as_deref(),
        ),
        _ => return None,
    };
    let scrubbed = scrub_request(method, url, headers, body);
    Some(MatchKey {
        kind,
        method: scrubbed.method,
        url: scrubbed.url,
        body_sha256: scrubbed.body_sha256,
        conditional: conditional_headers(&scrubbed.headers),
    })
}

/// Reduce a stored (already-scrubbed) [`TapeRequest`] to its [`MatchKey`]. The
/// body digest derives from the stored body, decoded byte-faithfully, so it
/// equals the digest the recorder computed from the raw request body.
fn match_key_for_request(
    kind: TapeKind,
    request: &TapeRequest,
    sidecar_dir: &Path,
) -> Result<MatchKey, TapeError> {
    let body_sha256 = match &request.body {
        Some(body) => {
            let bytes = decode_body(body, sidecar_dir)?;
            (!bytes.is_empty()).then(|| sha256_hex(&bytes))
        },
        None => None,
    };
    Ok(MatchKey {
        kind,
        method: request.method.clone(),
        url: request.url.clone(),
        body_sha256,
        conditional: conditional_headers(&request.headers),
    })
}

fn conditional_headers(headers: &[TapeHeader]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter_map(|h| {
            let lower = h.name.to_ascii_lowercase();
            CONDITIONAL_HEADERS
                .contains(&lower.as_str())
                .then(|| (lower, h.value.clone()))
        })
        .collect();
    out.sort();
    out
}

/// Decode a tape body, reporting a missing sidecar as [`TapeError::SidecarMissing`]
/// rather than a bare io error.
fn decode_body(body: &TapeBody, sidecar_dir: &Path) -> Result<Vec<u8>, TapeError> {
    if let TapeBody::Sidecar { sha256, .. } = body
        && !sidecar_dir.join(format!("{sha256}.bin")).exists()
    {
        return Err(TapeError::SidecarMissing {
            sha256: sha256.clone(),
        });
    }
    body.decode(sidecar_dir).map_err(TapeError::Io)
}

fn wit_headers(headers: Vec<TapeHeader>) -> Vec<wit::Header> {
    headers
        .into_iter()
        .map(|h| wit::Header {
            name: h.name,
            value: h.value,
        })
        .collect()
}

/// Parse the Debug rendering of a wit `ErrorKind` back into the enum. An
/// unrecognized kind means a corrupt or forward-versioned tape.
fn parse_error_kind(kind: &str) -> Result<wit::ErrorKind, TapeError> {
    use wit::ErrorKind::{
        Denied, Internal, InvalidInput, Network, NotADirectory, NotAFile, NotFound,
        PermissionDenied, RateLimited, Timeout, TooLarge, VersionMismatch,
    };
    Ok(match kind {
        "NotFound" => NotFound,
        "NotADirectory" => NotADirectory,
        "NotAFile" => NotAFile,
        "PermissionDenied" => PermissionDenied,
        "InvalidInput" => InvalidInput,
        "TooLarge" => TooLarge,
        "Network" => Network,
        "Timeout" => Timeout,
        "Denied" => Denied,
        "RateLimited" => RateLimited,
        "VersionMismatch" => VersionMismatch,
        "Internal" => Internal,
        other => return Err(corrupt(format!("unknown tape error kind {other:?}"))),
    })
}

/// Host and path of a scrubbed url, used to group nearest candidates by
/// method + host + path (query ignored). An unparseable url degrades to empty
/// host + the raw string as path so grouping still behaves deterministically.
fn host_and_path(raw: &str) -> (String, String) {
    match url::Url::parse(raw) {
        Ok(parsed) => (
            parsed.host_str().unwrap_or_default().to_owned(),
            parsed.path().to_owned(),
        ),
        Err(_) => (String::new(), raw.to_owned()),
    }
}

/// Derive `(provider, scenario)` from `tests/<provider>/tapes/<scenario>.jsonl`.
fn provider_scenario(tape_path: &Path) -> (String, String) {
    let scenario = tape_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let provider = tape_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (provider, scenario)
}

/// Render the tape-miss report. Free function so the exact format is unit-
/// testable without a loaded tape.
fn render_miss_report(
    tape_path: &Path,
    pending_kind: TapeKind,
    pending_method: &str,
    pending_url: &str,
    catalog: &[Candidate],
    consumed: &HashSet<u32>,
) -> String {
    let (pending_host, pending_path) = host_and_path(pending_url);
    let mut nearest: Vec<&Candidate> = catalog
        .iter()
        .filter(|candidate| {
            candidate.kind == pending_kind && candidate.method == pending_method && {
                let (host, path) = host_and_path(&candidate.url);
                host == pending_host && path == pending_path
            }
        })
        .collect();
    nearest.sort_by_key(|candidate| candidate.seq);

    let prefixes: Vec<String> = nearest
        .iter()
        .map(|candidate| {
            format!(
                "seq {:02}: {} {}",
                candidate.seq, candidate.method, candidate.url
            )
        })
        .collect();
    let width = prefixes.iter().map(String::len).max().unwrap_or(0);

    let (provider, scenario) = provider_scenario(tape_path);

    let mut out = String::new();
    let _ = writeln!(out, "tape miss: no recorded exchange matches this callout");
    let _ = writeln!(out, "  scenario tape: {}", tape_path.display());
    let _ = writeln!(out, "  pending: {pending_method} {pending_url}");
    let _ = writeln!(out, "  nearest candidates (same method + host + path):");
    for (candidate, prefix) in nearest.iter().zip(&prefixes) {
        let state = if consumed.contains(&candidate.seq) {
            "[consumed]"
        } else {
            "[available]"
        };
        let _ = writeln!(out, "    {prefix:<width$} {state}");
    }
    let _ = writeln!(
        out,
        "  hint: if the provider's request shape changed intentionally, re-record:"
    );
    let _ = write!(out, "        just itest record {provider} {scenario}");
    out
}

fn corrupt(message: impl Into<String>) -> TapeError {
    TapeError::Io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_cache() -> (tempfile::TempDir, BlobCache) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = BlobCache::new(dir.path().join("blobs"));
        (dir, cache)
    }

    fn text_http(status: u16, body: &str) -> TapeResponse {
        TapeResponse::Http {
            status,
            headers: vec![],
            body: TapeBody::Text {
                lines: body.split('\n').map(str::to_owned).collect(),
            },
        }
    }

    fn get_request(url: &str, headers: Vec<TapeHeader>) -> TapeRequest {
        TapeRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers,
            body: None,
            cache_key: None,
        }
    }

    fn get_callout(url: &str, headers: Vec<wit::Header>) -> wit::Callout {
        wit::Callout::Fetch(wit::HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers,
            body: None,
        })
    }

    /// Build a player directly from entries, keyed as `load` would, without a
    /// tape file on disk.
    fn player_from(entries: Vec<TapeEntry>, sidecar_dir: PathBuf) -> TapePlayer {
        let catalog = entries
            .iter()
            .map(|entry| Candidate {
                seq: entry.seq,
                kind: entry.kind,
                method: entry.request.method.clone(),
                url: entry.request.url.clone(),
            })
            .collect();
        let mut queues: HashMap<MatchKey, VecDeque<TapeEntry>> = HashMap::new();
        for entry in entries {
            let key =
                match_key_for_request(entry.kind, &entry.request, &sidecar_dir).expect("match key");
            queues.entry(key).or_default().push_back(entry);
        }
        TapePlayer {
            queues,
            sidecar_dir,
            tape_path: PathBuf::from("tests/example/tapes/scenario.jsonl"),
            catalog,
            consumed: HashSet::new(),
        }
    }

    fn entry(seq: u32, request: TapeRequest, response: TapeResponse) -> TapeEntry {
        TapeEntry {
            seq,
            kind: TapeKind::HttpFetch,
            request,
            response,
        }
    }

    #[test]
    fn identical_keys_answer_fifo() {
        let (dir, blobs) = blob_cache();
        let url = "https://api.example.com/x";
        let entries = vec![
            entry(0, get_request(url, vec![]), text_http(200, "first")),
            entry(1, get_request(url, vec![]), text_http(200, "second")),
        ];
        let mut player = player_from(entries, dir.path().join("blobs"));

        let callout = get_callout(url, vec![]);
        let first = player.answer(&callout, &blobs).expect("first answer");
        let second = player.answer(&callout, &blobs).expect("second answer");

        assert_eq!(http_body(&first), b"first");
        assert_eq!(http_body(&second), b"second");
        assert_eq!(player.remaining(), 0);
    }

    #[test]
    fn conditional_header_discriminates_entries() {
        let (dir, blobs) = blob_cache();
        let url = "https://api.example.com/resource";
        let plain = entry(0, get_request(url, vec![]), text_http(200, "fresh body"));
        let conditional = entry(
            1,
            get_request(
                url,
                vec![TapeHeader {
                    name: "if-none-match".to_owned(),
                    value: "\"v1\"".to_owned(),
                }],
            ),
            text_http(304, ""),
        );
        let mut player = player_from(vec![plain, conditional], dir.path().join("blobs"));

        // A plain GET resolves to the unconditional 200 entry.
        let plain_result = player
            .answer(&get_callout(url, vec![]), &blobs)
            .expect("plain answer");
        assert_eq!(http_status(&plain_result), 200);

        // The same GET carrying If-None-Match resolves to the 304 entry.
        let conditional_result = player
            .answer(
                &get_callout(
                    url,
                    vec![wit::Header {
                        name: "If-None-Match".to_owned(),
                        value: "\"v1\"".to_owned(),
                    }],
                ),
                &blobs,
            )
            .expect("conditional answer");
        assert_eq!(http_status(&conditional_result), 304);
    }

    #[test]
    fn blob_response_rematerializes_recorded_bytes() {
        let (dir, blobs) = blob_cache();
        let recorded = b"blob payload bytes";
        let request = TapeRequest {
            method: "GET".to_owned(),
            url: "https://cdn.example.com/pkg.bin".to_owned(),
            headers: vec![],
            body: None,
            cache_key: Some("pkg/pkg.bin".to_owned()),
        };
        let response = TapeResponse::Blob {
            status: 200,
            content_type: Some("application/octet-stream".to_owned()),
            etag: Some("etag-1".to_owned()),
            response_headers: vec![],
            body: TapeBody::Base64 {
                data: {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(recorded)
                },
            },
        };
        let mut player = player_from(
            vec![TapeEntry {
                seq: 0,
                kind: TapeKind::BlobFetch,
                request,
                response,
            }],
            dir.path().join("blobs"),
        );

        let callout = wit::Callout::FetchBlob(wit::BlobFetchRequest {
            method: "GET".to_owned(),
            url: "https://cdn.example.com/pkg.bin".to_owned(),
            headers: vec![],
            body: None,
            cache_key: "pkg/pkg.bin".to_owned(),
        });
        let result = player.answer(&callout, &blobs).expect("blob answer");
        let wit::CalloutResult::BlobFetched(fetched) = result else {
            panic!("expected BlobFetched");
        };
        assert_eq!(fetched.size, recorded.len() as u64);
        // The minted id resolves to the recorded bytes in the blob cache.
        assert_eq!(
            blobs.bytes_for_tests(fetched.blob).as_deref(),
            Some(recorded.as_slice())
        );
    }

    #[test]
    fn miss_report_matches_expected_format() {
        let tape_path = Path::new("tests/github/tapes/repo-browse.jsonl");
        let catalog = vec![
            Candidate {
                seq: 7,
                kind: TapeKind::HttpFetch,
                method: "GET".to_owned(),
                url: "https://api.github.com/repos/o/r/issues".to_owned(),
            },
            Candidate {
                seq: 12,
                kind: TapeKind::HttpFetch,
                method: "GET".to_owned(),
                url: "https://api.github.com/repos/o/r/issues?page=1".to_owned(),
            },
            // Different path: excluded from the nearest-candidate list.
            Candidate {
                seq: 3,
                kind: TapeKind::HttpFetch,
                method: "GET".to_owned(),
                url: "https://api.github.com/repos/o/r/pulls".to_owned(),
            },
        ];
        let consumed = HashSet::from([7]);
        let rendered = render_miss_report(
            tape_path,
            TapeKind::HttpFetch,
            "GET",
            "https://api.github.com/repos/o/r/issues?page=2",
            &catalog,
            &consumed,
        );
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn missing_sidecar_is_reported() {
        let (dir, blobs) = blob_cache();
        let request = get_request("https://api.example.com/big", vec![]);
        let response = TapeResponse::Http {
            status: 200,
            headers: vec![],
            body: TapeBody::Sidecar {
                sha256: "deadbeef".to_owned(),
                size: 10,
            },
        };
        let mut player = player_from(vec![entry(0, request, response)], dir.path().join("blobs"));
        let err = player
            .answer(&get_callout("https://api.example.com/big", vec![]), &blobs)
            .expect_err("missing sidecar must error");
        assert!(matches!(err, TapeError::SidecarMissing { sha256 } if sha256 == "deadbeef"));
    }

    fn http_body(result: &wit::CalloutResult) -> Vec<u8> {
        match result {
            wit::CalloutResult::HttpResponse(r) => r.body.clone(),
            other => panic!("expected HttpResponse, got {other:?}"),
        }
    }

    fn http_status(result: &wit::CalloutResult) -> u16 {
        match result {
            wit::CalloutResult::HttpResponse(r) => r.status,
            other => panic!("expected HttpResponse, got {other:?}"),
        }
    }
}
