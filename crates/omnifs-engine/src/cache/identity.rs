//! Stable, domain-separated identities for host-owned callout storage.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlobRequestId([u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlobGeneration([u8; 32]);

/// Identity for a mount-scoped Git checkout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GitId([u8; 32]);

impl BlobRequestId {
    /// Hash validated request material. Host-injected auth is deliberately
    /// absent because it is not part of the provider request identity.
    pub(crate) fn new(
        method: &str,
        canonical_url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Self {
        let mut body_material = Vec::new();
        match body {
            Some(bytes) => {
                body_material.push(1);
                frame_bytes(&mut body_material, bytes);
            },
            None => body_material.push(0),
        }
        Self(hash_parts(
            b"omnifs/blob-request/v1",
            &[
                method.as_bytes(),
                canonical_url.as_bytes(),
                &encode_headers(headers),
                &body_material,
            ],
        ))
    }

    pub(crate) fn from_hex(value: &str) -> Option<Self> {
        parse_hex(value).map(Self)
    }

    pub(crate) fn filesystem_name(self) -> String {
        hex(self.0)
    }
}

impl BlobGeneration {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub(crate) fn from_hash(hash: blake3::Hash) -> Self {
        Self(*hash.as_bytes())
    }

    pub(crate) fn from_hex(value: &str) -> Option<Self> {
        parse_hex(value).map(Self)
    }

    pub(crate) fn filesystem_name(self) -> String {
        hex(self.0)
    }
}

impl GitId {
    pub(crate) fn new(mount_scope: &str, canonical_remote: &str, reference: Option<&str>) -> Self {
        let mut reference_material = Vec::new();
        match reference {
            Some(value) => {
                reference_material.push(1);
                frame_bytes(&mut reference_material, value.as_bytes());
            },
            None => reference_material.push(0),
        }
        Self(hash_parts(
            b"omnifs/git/v1",
            &[
                mount_scope.as_bytes(),
                canonical_remote.as_bytes(),
                &reference_material,
            ],
        ))
    }

    pub(crate) fn filesystem_name(&self) -> String {
        hex(self.0)
    }
}

impl fmt::Display for BlobRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.filesystem_name())
    }
}

impl fmt::Display for BlobGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.filesystem_name())
    }
}

impl fmt::Display for GitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.filesystem_name())
    }
}

fn hash_parts(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, domain);
    for part in parts {
        frame(&mut hasher, part);
    }
    *hasher.finalize().as_bytes()
}

fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn encode_headers(headers: &[(String, String)]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        frame_bytes(&mut encoded, name.as_bytes());
        frame_bytes(&mut encoded, value.as_bytes());
    }
    encoded
}

fn frame_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn parse_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let mut output = [0u8; 32];
    hex::decode_to_slice(value, &mut output).ok()?;
    Some(output)
}

fn hex(bytes: [u8; 32]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_unambiguous_and_mount_scoped() {
        assert_ne!(
            BlobRequestId::new(None, "GET", "https://example.test/A", &[], None),
            BlobRequestId::new(None, "GET", "https://example.test/a", &[], None),
        );
        assert_ne!(
            BlobRequestId::new(None, "POST", "https://example.test", &[], None),
            BlobRequestId::new(None, "POST", "https://example.test", &[], Some(b"")),
        );
        assert_ne!(
            GitId::new("mount-a", "remote", Some("main")),
            GitId::new("mount-b", "remote", Some("main")),
        );
        assert_ne!(
            GitId::new("mount", "remote", None),
            GitId::new("mount", "remote", Some("")),
        );
    }

    #[test]
    fn credential_partitions_reuse_without_cross_account_sharing() {
        let first = CredentialId::new("github", "oauth", "alice").unwrap();
        let second = CredentialId::new("github", "oauth", "bob").unwrap();
        let same_request =
            |auth| BlobRequestId::new(auth, "GET", "https://api.example.test/data", &[], None);

        assert_eq!(same_request(Some(&first)), same_request(Some(&first)));
        assert_ne!(same_request(Some(&first)), same_request(Some(&second)));
        assert_ne!(same_request(None), same_request(Some(&first)));
    }

    #[test]
    fn blob_generation_is_content_addressed() {
        assert_eq!(
            BlobGeneration::from_bytes(b"body"),
            BlobGeneration::from_bytes(b"body")
        );
        assert_ne!(
            BlobGeneration::from_bytes(b"body"),
            BlobGeneration::from_bytes(b"other")
        );
    }
}
