//! Opaque `ObjectId` body: `postcard(logical-id)`. The host forms these bytes from a
//! provider-supplied `logical-id` and reverses them when pushing a stored canonical
//! back for a warm read. The host never inspects the contents.

use omnifs_wit::provider::types as wit_types;

/// A stable, serializable mirror of `wit_types::LogicalId` for postcard encoding.
#[derive(serde::Serialize, serde::Deserialize)]
struct LogicalIdWire {
    kind: String,
    captures: Vec<(String, String)>,
}

/// The opaque `ObjectId` body the cache keys on.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ObjectId(Vec<u8>);

impl ObjectId {
    pub fn from_wit(id: &wit_types::LogicalId) -> Self {
        let wire = LogicalIdWire {
            kind: id.kind.clone(),
            captures: id
                .captures
                .iter()
                .map(|c| (c.name.clone(), c.value.clone()))
                .collect(),
        };
        // postcard of a fixed-order struct is deterministic; infallible for owned data.
        Self(postcard::to_allocvec(&wire).expect("postcard logical-id encode is infallible"))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn to_wit(&self) -> Option<wit_types::LogicalId> {
        let wire: LogicalIdWire = postcard::from_bytes(&self.0).ok()?;
        Some(wit_types::LogicalId {
            kind: wire.kind,
            captures: wire
                .captures
                .into_iter()
                .map(|(name, value)| wit_types::IdCapture { name, value })
                .collect(),
        })
    }
}
