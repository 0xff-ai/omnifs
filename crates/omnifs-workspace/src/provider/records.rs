use serde::{Deserialize, Serialize};

pub const TAG_HANDLER: u8 = 0x01;
pub const TAG_MUTATION: u8 = 0x02;
pub const TAG_SUBTREE_ROUTE: u8 = 0x03;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HandlerKindRecord {
    Dir,
    File,
    TreeRef,
    /// Bind site: the handler dispatches a typed subtree implemented in
    /// a `#[subtree] impl B { ... }` block. `subtree_type` on the record
    /// names the bindings type `B`; the resolver joins this site against
    /// `SubtreeRouteRecord`s with the same `subtree_type`.
    Subtree,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestCaptureRecord {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandlerRecord {
    pub path_template: String,
    pub handler_name: String,
    pub handler_kind: HandlerKindRecord,
    pub capture_schema: Vec<ManifestCaptureRecord>,
    /// Set when `handler_kind == Subtree`: identifies the subtree
    /// bindings type so the resolver can pair the site with its inner
    /// `SubtreeRouteRecord`s. Always `None` for other kinds.
    pub subtree_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationRecord {
    pub path_template: String,
    pub capture_schema: Vec<ManifestCaptureRecord>,
}

/// A path handler declared inside a `#[subtree] impl B { ... }` block.
/// Templates are *relative to the bind site* the resolver pairs the
/// record with via `subtree_type`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubtreeRouteRecord {
    pub subtree_type: String,
    pub path_template: String,
    pub handler_name: String,
    /// Subtree routes are always `Dir` or `File`. Treerefs and binds
    /// are not allowed inside a subtree.
    pub handler_kind: HandlerKindRecord,
    pub capture_schema: Vec<ManifestCaptureRecord>,
}

pub struct ManifestRecordIter<'a> {
    rest: &'a [u8],
}

impl<'a> ManifestRecordIter<'a> {
    #[must_use]
    pub fn new(section: &'a [u8]) -> Self {
        Self { rest: section }
    }
}

#[derive(Clone, Debug)]
pub enum ManifestRecord {
    Handler(HandlerRecord),
    Mutation(MutationRecord),
    SubtreeRoute(SubtreeRouteRecord),
    Unknown { tag: u8, body: Vec<u8> },
}

impl Iterator for ManifestRecordIter<'_> {
    type Item = Result<ManifestRecord, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.len() < 6 {
            return Some(Err(DecodeError::Truncated));
        }
        let len_bytes: [u8; 4] = self.rest[0..4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len < 2 || self.rest.len() < 4 + len {
            return Some(Err(DecodeError::Truncated));
        }
        let tag = self.rest[4];
        let body = &self.rest[6..4 + len];
        self.rest = &self.rest[4 + len..];
        Some(decode_manifest_one(tag, body))
    }
}

fn decode_manifest_one(tag: u8, body: &[u8]) -> Result<ManifestRecord, DecodeError> {
    match tag {
        TAG_HANDLER => serde_json::from_slice(body)
            .map(ManifestRecord::Handler)
            .map_err(DecodeError::Json),
        TAG_MUTATION => serde_json::from_slice(body)
            .map(ManifestRecord::Mutation)
            .map_err(DecodeError::Json),
        TAG_SUBTREE_ROUTE => serde_json::from_slice(body)
            .map(ManifestRecord::SubtreeRoute)
            .map_err(DecodeError::Json),
        other => Ok(ManifestRecord::Unknown {
            tag: other,
            body: body.to_vec(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("truncated record in provider manifest section")]
    Truncated,
    #[error("json decode error: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::{
        HandlerKindRecord, HandlerRecord, ManifestRecord, ManifestRecordIter, TAG_HANDLER,
    };

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len() + 2).expect("record body + header fits u32");
        let mut framed = Vec::with_capacity(4 + body.len() + 2);
        framed.extend_from_slice(&len.to_le_bytes());
        framed.extend_from_slice(&[tag, 0]);
        framed.extend_from_slice(body);
        framed
    }

    #[test]
    fn manifest_record_iter_tolerates_unknown_tag() {
        let mut bytes = frame(0xEF, b"arbitrary");
        let handler = serde_json::to_vec(&HandlerRecord {
            path_template: "/".to_string(),
            handler_name: "Root".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();
        bytes.extend_from_slice(&frame(TAG_HANDLER, &handler));

        let mut iter = ManifestRecordIter::new(&bytes);
        match iter.next().unwrap().unwrap() {
            ManifestRecord::Unknown { tag: 0xEF, body } => {
                assert_eq!(body, b"arbitrary");
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(matches!(
            iter.next().unwrap().unwrap(),
            ManifestRecord::Handler(handler) if handler.handler_name == "Root"
        ));
    }
}
