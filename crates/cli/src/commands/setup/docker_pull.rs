use crate::runtime::Runtime;
use anyhow::Context;

/// Eagerly pull `image` from its registry, showing layered progress.
///
/// No-op-fast when the image is already cached locally.
pub async fn pull(runtime: &Runtime, image: &str) -> anyhow::Result<()> {
    runtime
        .pull_image_with_progress(image)
        .await
        .with_context(|| format!("pull image `{image}`"))
}
