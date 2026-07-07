//! Callout tape format: the on-disk record/replay representation.
//!
//! A tape is one `.jsonl` file per scenario, one [`TapeEntry`] per line. The
//! format is optimized for reviewable diffs: `UTF-8` bodies are stored as line
//! arrays so a re-record shows a real textual diff, and only genuinely large or
//! binary bodies spill to content-addressed sidecar blobs.

pub mod record;
pub mod replay;
pub mod scrub;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

/// `UTF-8` bodies up to this length are stored inline as [`TapeBody::Text`].
const TEXT_MAX: usize = 262_144; // 256 KiB
/// Bodies up to this length (that are not stored as text) inline as base64.
const BASE64_MAX: usize = 65_536; // 64 KiB

/// One recorded callout exchange. Serialized as a single `JSON` line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TapeEntry {
    /// Monotonic recording order, 0-based. Replay uses it only for `FIFO`
    /// tie-breaking among identical match keys and for error reporting.
    pub seq: u32,
    pub kind: TapeKind,
    pub request: TapeRequest,
    pub response: TapeResponse,
}

/// Which callout family an entry recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapeKind {
    /// `wit` `Callout::Fetch`.
    HttpFetch,
    /// `wit` `Callout::FetchBlob`.
    BlobFetch,
}

/// The outbound request, already scrubbed for persistence and matching.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TapeRequest {
    pub method: String,
    /// Scrubbed `URL`: values of sensitive query params replaced with
    /// `<scrubbed>` (param names kept). Never contains credentials.
    pub url: String,
    /// Scrubbed headers: values of sensitive headers replaced with
    /// `<scrubbed>` (names kept, order kept).
    pub headers: Vec<TapeHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<TapeBody>,
    /// Present only for `kind = BlobFetch` (the `wit` `cache-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}

/// A header name/value pair. Distinct from the `wit` `Header` so tape shapes
/// stay stable across `wit` regeneration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TapeHeader {
    pub name: String,
    pub value: String,
}

/// The recorded outcome of a callout.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapeResponse {
    Http {
        status: u16,
        headers: Vec<TapeHeader>,
        body: TapeBody,
    },
    /// Recorded `FetchBlob` outcome. Body bytes live in the sidecar so replay
    /// can rematerialize the blob; the `blob` id is NOT recorded (it is
    /// runtime-local).
    Blob {
        status: u16,
        content_type: Option<String>,
        etag: Option<String>,
        response_headers: Vec<TapeHeader>,
        body: TapeBody,
    },
    Error {
        /// Debug rendering of the `wit` `ErrorKind`, e.g. `"Network"`.
        kind: String,
        message: String,
        retryable: bool,
    },
}

/// A body payload, tiered by size and encoding so that reviewable text stays
/// inline and large or binary bytes spill to a sidecar.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapeBody {
    Empty,
    /// `UTF-8` body stored as lines for reviewable diffs. Byte-faithful
    /// reconstruction is `lines.join("\n")`. The split uses `str::split('\n')`
    /// (NOT `str::lines()`, which eats the trailing-newline distinction).
    Text {
        lines: Vec<String>,
    },
    /// Non-`UTF-8` body up to the sidecar threshold.
    Base64 {
        data: String,
    },
    /// Body stored out-of-line at `tapes/blobs/<sha256-hex>.bin`.
    Sidecar {
        sha256: String,
        size: u64,
    },
}

impl TapeBody {
    /// Encode raw bytes into the smallest reviewable representation. Writes a
    /// sidecar blob (content-addressed, only if absent) when the body is too
    /// large or not `UTF-8`-representable inline.
    ///
    /// `sidecar_dir` is the `tapes/blobs` directory; it is created on demand.
    pub fn encode(bytes: &[u8], sidecar_dir: &Path) -> io::Result<TapeBody> {
        if bytes.is_empty() {
            return Ok(TapeBody::Empty);
        }
        // Text tier is gated on both size and valid UTF-8; a large UTF-8 body
        // falls through to the sidecar rather than to base64.
        if bytes.len() <= TEXT_MAX
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            let lines = text.split('\n').map(str::to_owned).collect();
            return Ok(TapeBody::Text { lines });
        }
        if bytes.len() <= BASE64_MAX {
            return Ok(TapeBody::Base64 {
                data: BASE64.encode(bytes),
            });
        }
        let sha256 = sha256_hex(bytes);
        let path = sidecar_dir.join(format!("{sha256}.bin"));
        // Content-addressed: identical bytes share one blob across scenarios, so
        // skip the write when the blob already exists.
        if !path.exists() {
            fs::create_dir_all(sidecar_dir)?;
            write_atomic(&path, bytes)?;
        }
        Ok(TapeBody::Sidecar {
            sha256,
            size: bytes.len() as u64,
        })
    }

    /// Reconstruct the original bytes. `sidecar_dir` is consulted only for the
    /// [`TapeBody::Sidecar`] variant.
    pub fn decode(&self, sidecar_dir: &Path) -> io::Result<Vec<u8>> {
        match self {
            TapeBody::Empty => Ok(Vec::new()),
            TapeBody::Text { lines } => Ok(lines.join("\n").into_bytes()),
            TapeBody::Base64 { data } => BASE64
                .decode(data)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            TapeBody::Sidecar { sha256, .. } => fs::read(sidecar_dir.join(format!("{sha256}.bin"))),
        }
    }
}

/// A loaded tape: the ordered entries of one scenario file.
#[derive(Debug, Clone, Default)]
pub struct Tape {
    pub entries: Vec<TapeEntry>,
}

impl Tape {
    /// Parse a `.jsonl` tape. Blank lines are skipped; a malformed line reports
    /// its 1-based line number.
    pub fn load(path: &Path) -> Result<Tape, TapeError> {
        let content = fs::read_to_string(path)?;
        let mut entries = Vec::new();
        for (index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry = serde_json::from_str(line).map_err(|source| TapeError::Parse {
                line: index + 1,
                source,
            })?;
            entries.push(entry);
        }
        Ok(Tape { entries })
    }

    /// Write entries atomically (temp file + rename), one `serde_json` line per
    /// entry.
    pub fn save(path: &Path, entries: &[TapeEntry]) -> Result<(), TapeError> {
        let mut buf = String::new();
        for entry in entries {
            // Serializing plain data never fails in practice; surface it as an
            // io-category error rather than panicking.
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            buf.push_str(&line);
            buf.push('\n');
        }
        write_atomic(path, buf.as_bytes())?;
        Ok(())
    }
}

/// Failures reading, writing, or replaying a tape.
#[derive(Debug, thiserror::Error)]
pub enum TapeError {
    #[error("tape io error: {0}")]
    Io(#[from] io::Error),
    #[error("tape parse error at line {line}: {source}")]
    Parse {
        line: usize,
        source: serde_json::Error,
    },
    #[error("{rendered}")]
    Unmatched { rendered: String },
    #[error("sidecar blob missing: {sha256}.bin")]
    SidecarMissing { sha256: String },
}

/// Lowercase hex `SHA-256` of `bytes`. Shared by body content-addressing and
/// request-body match keys.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Two lowercase hex chars per byte.
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap());
    }
    out
}

/// Write `bytes` to `path` atomically via a same-directory temp file + rename,
/// so a crashed writer never leaves a half-written tape or blob.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8], sidecar_dir: &Path) -> Vec<u8> {
        let body = TapeBody::encode(bytes, sidecar_dir).expect("encode");
        body.decode(sidecar_dir).expect("decode")
    }

    #[test]
    fn text_bodies_preserve_trailing_newlines() {
        let dir = tempfile::tempdir().expect("tempdir");
        for case in ["a", "a\n", "a\n\n", "a\r\nb\n", "\r\n", ""] {
            let bytes = case.as_bytes();
            assert_eq!(roundtrip(bytes, dir.path()), bytes, "case {case:?}");
        }
    }

    #[test]
    fn empty_body_encodes_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            TapeBody::encode(b"", dir.path()).expect("encode"),
            TapeBody::Empty
        );
    }

    #[test]
    fn text_tier_boundary_at_256_kib() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Exactly 256 KiB of ASCII stays inline as text.
        let at = vec![b'x'; TEXT_MAX];
        assert!(matches!(
            TapeBody::encode(&at, dir.path()).expect("encode"),
            TapeBody::Text { .. }
        ));
        // One byte over the text ceiling is UTF-8 but too large for text, and
        // also over the base64 ceiling, so it spills to a sidecar.
        let over = vec![b'x'; TEXT_MAX + 1];
        assert!(matches!(
            TapeBody::encode(&over, dir.path()).expect("encode"),
            TapeBody::Sidecar { .. }
        ));
        assert_eq!(roundtrip(&over, dir.path()), over);
    }

    #[test]
    fn base64_tier_boundary_at_64_kib() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Non-UTF-8 bytes exactly at 64 KiB inline as base64.
        let at = vec![0xffu8; BASE64_MAX];
        assert!(matches!(
            TapeBody::encode(&at, dir.path()).expect("encode"),
            TapeBody::Base64 { .. }
        ));
        assert_eq!(roundtrip(&at, dir.path()), at);
        // One non-UTF-8 byte over the base64 ceiling spills to a sidecar.
        let over = vec![0xffu8; BASE64_MAX + 1];
        assert!(matches!(
            TapeBody::encode(&over, dir.path()).expect("encode"),
            TapeBody::Sidecar { .. }
        ));
        assert_eq!(roundtrip(&over, dir.path()), over);
    }

    #[test]
    fn sidecar_is_content_addressed_and_shared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = vec![0xabu8; TEXT_MAX + 10];
        let first = TapeBody::encode(&bytes, dir.path()).expect("encode");
        let second = TapeBody::encode(&bytes, dir.path()).expect("encode");
        assert_eq!(first, second, "same bytes must yield the same sidecar");
        let TapeBody::Sidecar { sha256, .. } = &first else {
            panic!("expected sidecar");
        };
        // Exactly one blob on disk for the shared bytes.
        let blobs: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "bin"))
            .collect();
        assert_eq!(blobs.len(), 1, "identical bytes must share one blob");
        assert!(dir.path().join(format!("{sha256}.bin")).exists());
    }

    #[test]
    fn distinct_bytes_get_distinct_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = TapeBody::encode(&vec![0x01u8; TEXT_MAX + 1], dir.path()).expect("encode");
        let b = TapeBody::encode(&vec![0x02u8; TEXT_MAX + 1], dir.path()).expect("encode");
        assert_ne!(a, b);
    }

    #[test]
    fn save_load_roundtrips_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scenario.jsonl");
        let entries = vec![
            TapeEntry {
                seq: 0,
                kind: TapeKind::HttpFetch,
                request: TapeRequest {
                    method: "GET".into(),
                    url: "https://example.com/a".into(),
                    headers: vec![TapeHeader {
                        name: "accept".into(),
                        value: "application/json".into(),
                    }],
                    body: None,
                    cache_key: None,
                },
                response: TapeResponse::Http {
                    status: 200,
                    headers: vec![],
                    body: TapeBody::Text {
                        lines: vec!["hi".into()],
                    },
                },
            },
            TapeEntry {
                seq: 1,
                kind: TapeKind::BlobFetch,
                request: TapeRequest {
                    method: "GET".into(),
                    url: "https://example.com/b".into(),
                    headers: vec![],
                    body: None,
                    cache_key: Some("k".into()),
                },
                response: TapeResponse::Error {
                    kind: "Network".into(),
                    message: "boom".into(),
                    retryable: true,
                },
            },
        ];
        Tape::save(&path, &entries).expect("save");
        let loaded = Tape::load(&path).expect("load");
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].seq, 0);
        assert_eq!(loaded.entries[1].kind, TapeKind::BlobFetch);
    }

    #[test]
    fn load_reports_the_failing_line_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.jsonl");
        // Line 1 valid, line 2 malformed. Blank lines do not count against the
        // reported number because they are read, then skipped.
        let good = serde_json::to_string(&TapeEntry {
            seq: 0,
            kind: TapeKind::HttpFetch,
            request: TapeRequest {
                method: "GET".into(),
                url: "https://example.com".into(),
                headers: vec![],
                body: None,
                cache_key: None,
            },
            response: TapeResponse::Error {
                kind: "Network".into(),
                message: String::new(),
                retryable: false,
            },
        })
        .expect("serialize");
        fs::write(&path, format!("{good}\n{{not valid json\n")).expect("write");
        match Tape::load(&path) {
            Err(TapeError::Parse { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected parse error on line 2, got {other:?}"),
        }
    }
}
