//! Pull and cache the release-channel libkrun guest image.
//!
//! The OCI client owns reference parsing, registry authentication, manifest
//! handling, and digest checks. This module owns only Omnifs cache paths,
//! byte progress, decompression, and atomic publication.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use crate::fs_runtime::{Artifact, ImageRef, RuntimeEvent, RuntimeEventSink};

/// Ensure the release-channel guest image is present as a decompressed local
/// `.raw` file and return that immutable base path.
pub(crate) async fn ensure_guest_image(
    image: &ImageRef,
    images_dir: &Path,
    events: RuntimeEventSink,
) -> Result<PathBuf> {
    let reference = image
        .as_str()
        .parse::<Reference>()
        .with_context(|| format!("parse guest image reference `{image}`"))?;
    let tag = reference
        .tag()
        .context("guest image reference must use a tag")?
        .to_owned();
    std::fs::create_dir_all(images_dir)
        .with_context(|| format!("create {}", images_dir.display()))?;

    let raw_path = images_dir.join(format!("{tag}.raw"));
    if raw_path.is_file() {
        return Ok(raw_path);
    }

    let zst_path = images_dir.join(format!("{tag}.raw.zst"));
    let client = Client::default();
    if !zst_path.is_file() {
        download(&client, &reference, &zst_path, events.clone()).await?;
    }

    match decompress(&zst_path, &raw_path) {
        Ok(()) => Ok(raw_path),
        Err(decompress_error) => {
            events.emit(RuntimeEvent::ImageRetry {
                artifact: Artifact::GuestImage,
                path: zst_path.clone(),
                reason: format!("{decompress_error:#}"),
            });
            let _ = std::fs::remove_file(&zst_path);
            download(&client, &reference, &zst_path, events).await?;
            decompress(&zst_path, &raw_path)?;
            Ok(raw_path)
        },
    }
}

async fn download(
    client: &Client,
    reference: &Reference,
    destination: &Path,
    events: RuntimeEventSink,
) -> Result<()> {
    let tmp_path = part_path(destination);
    let result: Result<u64> = async {
        let (manifest, _) = client
            .pull_image_manifest(reference, &RegistryAuth::Anonymous)
            .await
            .context("pull guest image manifest")?;
        let [layer] = manifest.layers.as_slice() else {
            anyhow::bail!(
                "guest image manifest has {} layers; expected exactly one layer",
                manifest.layers.len()
            );
        };
        let total = u64::try_from(layer.size).context("guest image layer size is negative")?;
        let file = tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("create {}", tmp_path.display()))?;
        let mut output = ProgressWriter {
            file,
            completed: 0,
            total,
            source: reference.registry().to_owned(),
            events: events.clone(),
        };
        client
            .pull_blob(reference, layer, &mut output)
            .await
            .context("pull guest image layer")?;
        output
            .flush()
            .await
            .with_context(|| format!("flush {}", tmp_path.display()))?;
        anyhow::ensure!(
            output.completed == total,
            "guest image layer size mismatch: expected {total} bytes, got {}",
            output.completed
        );
        let completed = output.completed;
        drop(output);
        std::fs::rename(&tmp_path, destination).with_context(|| {
            format!("rename {} to {}", tmp_path.display(), destination.display())
        })?;
        Ok(completed)
    }
    .await;

    match result {
        Ok(completed) => {
            events.emit(RuntimeEvent::DownloadFinished {
                artifact: Artifact::GuestImage,
                reference: reference
                    .tag()
                    .expect("tag checked before download")
                    .to_owned(),
                completed_bytes: Some(completed),
            });
            Ok(())
        },
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            events.emit(RuntimeEvent::DownloadFailed {
                artifact: Artifact::GuestImage,
                reference: None,
            });
            Err(error)
        },
    }
}

struct ProgressWriter {
    file: tokio::fs::File,
    completed: u64,
    total: u64,
    source: String,
    events: RuntimeEventSink,
}

impl AsyncWrite for ProgressWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.file).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                self.completed += written as u64;
                self.events.emit(RuntimeEvent::Download {
                    artifact: Artifact::GuestImage,
                    completed_bytes: self.completed,
                    total_bytes: Some(self.total),
                    source: self.source.clone(),
                });
                Poll::Ready(Ok(written))
            },
            result => result,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_shutdown(context)
    }
}

/// Decompress via a `.part` sibling and publish only a complete image.
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

    #[test]
    fn guest_image_reference_preserves_registry_and_tag() {
        let reference = "ghcr.io/0xff-ai/omnifs-guest:0.5.0"
            .parse::<Reference>()
            .unwrap();
        assert_eq!(reference.registry(), "ghcr.io");
        assert_eq!(reference.repository(), "0xff-ai/omnifs-guest");
        assert_eq!(reference.tag(), Some("0.5.0"));
    }
}
