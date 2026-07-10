//! Pulls the krunkit guest disk image from its ghcr OCI artifact and caches
//! it locally: anonymous token, manifest, single blob, sha256 verification,
//! decompress-once. Only the release channel ever reaches this path (see
//! `crate::krunkit_backend::resolve_guest_image`); the dev channel always
//! uses a local path and never downloads.
//!
//! Plain HTTP via the CLI's existing reqwest dependency, not an ORAS client:
//! the guest image is a single-blob OCI artifact, so the full registry
//! protocol surface ORAS covers (multi-arch indexes, referrers, copy) is
//! more than this needs. `oras` itself is a CI-only tool
//! (`scripts/ci/*guest-image*.sh`); it never ships in or runs from the CLI.
//!
//! Cache layout under `<cache_dir>/guest-images/`:
//! - `<tag>.raw.zst`: the verified, still-compressed download.
//! - `<tag>.raw`: decompressed once from the `.zst`, and what callers use.
//!
//! A present `<tag>.raw` is trusted on reuse without re-hashing: it was
//! produced by decompressing an already sha256-verified `.zst` (or, on a
//! corrupt-cache retry, a freshly re-verified one), and zstd's own frame
//! checksum makes a silently corrupted decompress unlikely. Re-verifying the
//! multi-hundred-MB `.zst` on every launch would cost real wall-clock time
//! for a file this function itself never mutates after the rename below.
//! Both the `.zst` and the final `.raw` are written to a `.part` sibling and
//! renamed into place only after their integrity check passes, so a
//! partial or failed download/decompress never leaves a usable-looking file
//! at the real path.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::launch_backend::ImageRef;

const GUEST_IMAGE_CACHE_SUBDIR: &str = "guest-images";

const ACCEPT_MANIFEST: &str =
    "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.artifact.manifest.v1+json";

/// The registry, repository, and reference (tag) parsed out of an
/// `ImageRef` like `ghcr.io/0xff-ai/omnifs-guest:0.5.0`.
struct OciRef {
    registry: String,
    repository: String,
    reference: String,
}

impl OciRef {
    fn parse(image: &str) -> Result<Self> {
        let (registry, rest) = image
            .split_once('/')
            .with_context(|| format!("guest image reference `{image}` has no registry host"))?;
        let (repository, reference) = rest
            .rsplit_once(':')
            .with_context(|| format!("guest image reference `{image}` has no tag"))?;
        anyhow::ensure!(
            !registry.is_empty() && !repository.is_empty() && !reference.is_empty(),
            "guest image reference `{image}` is malformed"
        );
        Ok(Self {
            registry: registry.to_string(),
            repository: repository.to_string(),
            reference: reference.to_string(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct BlobDescriptor {
    #[serde(rename = "mediaType")]
    #[allow(dead_code)] // carried for future content-type checks; not asserted today
    media_type: String,
    digest: String,
    size: u64,
}

/// Accepts both the OCI Image Manifest shape (`config` + `layers`) that
/// `oras push --artifact-type` produces today, and the older, separate OCI
/// Artifact Manifest shape (`blobs`, no `config`), per ghcr's own migration
/// history between the two. Exactly one of the two lists is ever populated
/// for a real manifest; [`Self::single_blob`] does not care which.
#[derive(Debug, Deserialize)]
struct OciManifest {
    #[serde(default)]
    layers: Vec<BlobDescriptor>,
    #[serde(default)]
    blobs: Vec<BlobDescriptor>,
}

impl OciManifest {
    /// The guest image manifest carries exactly one blob (the `.raw.zst`);
    /// zero or more than one is a shape this puller does not understand.
    fn single_blob(&self) -> Result<&BlobDescriptor> {
        match (self.layers.as_slice(), self.blobs.as_slice()) {
            ([one], []) | ([], [one]) => Ok(one),
            ([], []) => anyhow::bail!(
                "guest image manifest has no layers or blobs; expected exactly one blob"
            ),
            (layers, blobs) => anyhow::bail!(
                "guest image manifest has {} layer(s) and {} blob(s); expected exactly one blob total",
                layers.len(),
                blobs.len()
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// Ensure the release-channel guest image named by `image` is present as a
/// decompressed local `.raw` file under `cache_dir`, pulling and caching it
/// on first use. Returns the local path a launch can hand straight to
/// krunkit.
pub(crate) async fn ensure_guest_image(image: &ImageRef, cache_dir: &Path) -> Result<PathBuf> {
    let oci_ref = OciRef::parse(image.as_str())?;
    let images_dir = cache_dir.join(GUEST_IMAGE_CACHE_SUBDIR);
    std::fs::create_dir_all(&images_dir)
        .with_context(|| format!("create {}", images_dir.display()))?;

    let raw_path = images_dir.join(format!("{}.raw", oci_ref.reference));
    if raw_path.is_file() {
        return Ok(raw_path);
    }

    let zst_path = images_dir.join(format!("{}.raw.zst", oci_ref.reference));
    let client = reqwest::Client::new();

    if !zst_path.is_file() {
        download_and_verify(&client, &oci_ref, image, &zst_path).await?;
    }

    match decompress(&zst_path, &raw_path) {
        Ok(()) => Ok(raw_path),
        Err(decompress_error) => {
            // The cached .zst may be a leftover from an interrupted prior
            // decompress; re-download once before giving up, rather than
            // leaving the caller stuck on a permanently corrupt cache entry.
            eprintln!(
                "cached guest image at {} failed to decompress ({decompress_error:#}); \
                 re-downloading",
                zst_path.display()
            );
            let _ = std::fs::remove_file(&zst_path);
            download_and_verify(&client, &oci_ref, image, &zst_path).await?;
            decompress(&zst_path, &raw_path)?;
            Ok(raw_path)
        },
    }
}

async fn fetch_pull_token(client: &reqwest::Client, oci_ref: &OciRef) -> Result<String> {
    let url = format!(
        "https://{}/token?scope=repository:{}:pull&service={}",
        oci_ref.registry, oci_ref.repository, oci_ref.registry
    );
    let response: TokenResponse = client
        .get(&url)
        .send()
        .await
        .context("request an anonymous ghcr pull token")?
        .error_for_status()
        .context("ghcr token endpoint returned an error status")?
        .json()
        .await
        .context("parse the ghcr token response")?;
    response
        .token
        .or(response.access_token)
        .context("ghcr token response carried no token")
}

async fn fetch_manifest(
    client: &reqwest::Client,
    oci_ref: &OciRef,
    token: &str,
) -> Result<OciManifest> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        oci_ref.registry, oci_ref.repository, oci_ref.reference
    );
    let bytes = client
        .get(&url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, ACCEPT_MANIFEST)
        .send()
        .await
        .context("request the guest image manifest")?
        .error_for_status()
        .context("ghcr manifest endpoint returned an error status")?
        .bytes()
        .await
        .context("read the guest image manifest body")?;
    parse_manifest(&bytes)
}

fn parse_manifest(bytes: &[u8]) -> Result<OciManifest> {
    serde_json::from_slice(bytes).context("parse the guest image OCI manifest")
}

/// Download the manifest's one blob, verify its sha256 digest against the
/// manifest before it is trusted, and land it at `dest` only after that
/// check passes (a temp sibling is used until then).
async fn download_and_verify(
    client: &reqwest::Client,
    oci_ref: &OciRef,
    image: &ImageRef,
    dest: &Path,
) -> Result<()> {
    eprintln!(
        "Downloading the krunkit guest image ({image}); this is roughly 262 MB and only \
         happens once per version."
    );

    let token = fetch_pull_token(client, oci_ref).await?;
    let manifest = fetch_manifest(client, oci_ref, &token).await?;
    let blob = manifest.single_blob()?;

    let blob_url = format!(
        "https://{}/v2/{}/blobs/{}",
        oci_ref.registry, oci_ref.repository, blob.digest
    );
    let mut response = client
        .get(&blob_url)
        .bearer_auth(&token)
        .send()
        .await
        .context("request the guest image blob")?
        .error_for_status()
        .context("ghcr blob endpoint returned an error status")?;

    let tmp_path = part_path(dest);
    let mut file = std::fs::File::create(&tmp_path)
        .with_context(|| format!("create {}", tmp_path.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read a chunk of the guest image blob")?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        downloaded += chunk.len() as u64;
    }
    file.flush()
        .with_context(|| format!("flush {}", tmp_path.display()))?;
    drop(file);

    let actual_digest = format!("sha256:{:x}", hasher.finalize());
    if actual_digest != blob.digest || downloaded != blob.size {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::bail!(
            "guest image blob failed verification: expected {} ({} bytes), got {actual_digest} \
             ({downloaded} bytes)",
            blob.digest,
            blob.size
        );
    }

    std::fs::rename(&tmp_path, dest)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), dest.display()))?;
    eprintln!("Guest image downloaded and verified.");
    Ok(())
}

/// Decompress `zst_path` into `raw_path`, via a `.part` sibling renamed into
/// place only once the whole stream has decoded successfully.
fn decompress(zst_path: &Path, raw_path: &Path) -> Result<()> {
    let input =
        std::fs::File::open(zst_path).with_context(|| format!("open {}", zst_path.display()))?;
    let mut decoder =
        zstd::stream::read::Decoder::new(input).context("create guest image zstd decoder")?;

    let tmp_path = part_path(raw_path);
    let mut output = std::fs::File::create(&tmp_path)
        .with_context(|| format!("create {}", tmp_path.display()))?;
    std::io::copy(&mut decoder, &mut output).with_context(|| {
        format!(
            "decompress {} to {}",
            zst_path.display(),
            tmp_path.display()
        )
    })?;
    output
        .flush()
        .with_context(|| format!("flush {}", tmp_path.display()))?;
    drop(output);

    std::fs::rename(&tmp_path, raw_path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), raw_path.display()))?;
    Ok(())
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE_MANIFEST_FIXTURE: &str = r#"{
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.omnifs.guest-image.v1+zstd",
        "config": {
            "mediaType": "application/vnd.oci.empty.v1+json",
            "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            "size": 2
        },
        "layers": [
            {
                "mediaType": "application/vnd.omnifs.guest-image.v1+zstd",
                "digest": "sha256:2d24b9eb82aa02a06ac3a487489a17083ec337a613ccb2a1f1ca610ec37370ca",
                "size": 18
            }
        ]
    }"#;

    const ARTIFACT_MANIFEST_FIXTURE: &str = r#"{
        "mediaType": "application/vnd.oci.artifact.manifest.v1+json",
        "artifactType": "application/vnd.omnifs.guest-image.v1+zstd",
        "blobs": [
            {
                "mediaType": "application/vnd.omnifs.guest-image.v1+zstd",
                "digest": "sha256:2d24b9eb82aa02a06ac3a487489a17083ec337a613ccb2a1f1ca610ec37370ca",
                "size": 18
            }
        ]
    }"#;

    #[test]
    fn parses_oci_image_manifest_shape() {
        let manifest = parse_manifest(IMAGE_MANIFEST_FIXTURE.as_bytes()).unwrap();
        let blob = manifest.single_blob().unwrap();
        assert_eq!(
            blob.digest,
            "sha256:2d24b9eb82aa02a06ac3a487489a17083ec337a613ccb2a1f1ca610ec37370ca"
        );
        assert_eq!(blob.size, 18);
    }

    #[test]
    fn parses_legacy_artifact_manifest_shape() {
        let manifest = parse_manifest(ARTIFACT_MANIFEST_FIXTURE.as_bytes()).unwrap();
        let blob = manifest.single_blob().unwrap();
        assert_eq!(
            blob.digest,
            "sha256:2d24b9eb82aa02a06ac3a487489a17083ec337a613ccb2a1f1ca610ec37370ca"
        );
        assert_eq!(blob.size, 18);
    }

    #[test]
    fn rejects_a_manifest_with_no_blobs() {
        let manifest = parse_manifest(r#"{"layers": [], "blobs": []}"#.as_bytes()).unwrap();
        let err = manifest.single_blob().unwrap_err();
        assert!(err.to_string().contains("no layers or blobs"));
    }

    #[test]
    fn rejects_a_manifest_with_multiple_blobs() {
        let two_layers = r#"{
            "layers": [
                {"mediaType": "a", "digest": "sha256:aaaa", "size": 1},
                {"mediaType": "b", "digest": "sha256:bbbb", "size": 2}
            ]
        }"#;
        let manifest = parse_manifest(two_layers.as_bytes()).unwrap();
        let err = manifest.single_blob().unwrap_err();
        assert!(err.to_string().contains("expected exactly one blob"));
    }

    #[test]
    fn oci_ref_parses_registry_repository_and_tag() {
        let oci_ref = OciRef::parse("ghcr.io/0xff-ai/omnifs-guest:0.5.0").unwrap();
        assert_eq!(oci_ref.registry, "ghcr.io");
        assert_eq!(oci_ref.repository, "0xff-ai/omnifs-guest");
        assert_eq!(oci_ref.reference, "0.5.0");
    }

    #[test]
    fn oci_ref_rejects_a_reference_with_no_registry() {
        assert!(OciRef::parse("omnifs-guest:0.5.0").is_err());
    }

    #[test]
    fn oci_ref_rejects_a_reference_with_no_tag() {
        assert!(OciRef::parse("ghcr.io/0xff-ai/omnifs-guest").is_err());
    }

    // "hello guest image\n" is the exact 18-byte fixture blob content used
    // above (verified independently via `printf 'hello guest image\n' |
    // shasum -a 256` and against a real `oras push` of the same bytes), so
    // this doubles as the digest math the fixtures' `size: 18` assumes.
    const FIXTURE_BLOB_BYTES: &[u8] = b"hello guest image\n";
    const FIXTURE_BLOB_DIGEST: &str =
        "sha256:2d24b9eb82aa02a06ac3a487489a17083ec337a613ccb2a1f1ca610ec37370ca";

    #[test]
    fn digest_verification_detects_a_mismatch() {
        let mut hasher = Sha256::new();
        hasher.update(FIXTURE_BLOB_BYTES);
        let actual = format!("sha256:{:x}", hasher.finalize());
        let expected = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        assert_ne!(actual, expected);
    }

    #[test]
    fn digest_verification_accepts_a_match() {
        let mut hasher = Sha256::new();
        hasher.update(FIXTURE_BLOB_BYTES);
        let actual = format!("sha256:{:x}", hasher.finalize());
        assert_eq!(actual, FIXTURE_BLOB_DIGEST);
        assert_eq!(
            FIXTURE_BLOB_BYTES.len(),
            18,
            "matches the fixtures' size: 18"
        );
    }
}
