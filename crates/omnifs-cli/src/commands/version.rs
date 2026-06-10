//! `omnifs version` — print the CLI version. `--detail` prints a richer
//! block listing image / container state / credential file / provider count /
//! configured dirs.

use anyhow::Result;
use bollard::Docker;
use clap::Args;

use crate::app_context::AppContext;
use crate::catalog::{ProviderCatalog, ProviderDirStatus};
use crate::image_ref::{ImageOrigin, ImageRef};
use crate::paths::Paths;

#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {
    /// Print extended version detail (CLI + image + container + store + provider count + dirs).
    #[arg(long = "detail")]
    pub detail: bool,
}

impl VersionArgs {
    pub async fn run(self) -> Result<()> {
        if !self.detail {
            anstream::println!("omnifs {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }

        let ctx = AppContext::resolve_default()?;
        let cli = env!("CARGO_PKG_VERSION");
        let container = describe_container(ctx.runtime().container_name().as_str()).await;
        let image = match container.image {
            Some(image) => ImageRef::new(image)?,
            None => ctx.runtime().image().clone(),
        };
        let image_location = describe_image_location(&image).await;
        let provider_status = provider_dir_summary(ctx.catalog());

        anstream::println!("CLI:        omnifs {cli}");
        anstream::println!("Image:      {image} ({image_location})");
        if image != *ctx.runtime().image() {
            anstream::println!("Configured: {}", ctx.runtime().image());
        }
        anstream::println!(
            "Container:  {} (`{}`)",
            container.state,
            ctx.runtime().container_name()
        );
        anstream::println!(
            "Store:      file ({})",
            Paths::display(&ctx.paths().credentials_file)
        );
        anstream::println!("Providers:  {provider_status}");
        anstream::println!();
        anstream::println!("Paths:");
        anstream::println!(
            "  config:       {}",
            Paths::display(&ctx.paths().config_dir)
        );
        anstream::println!("  cache:        {}", Paths::display(&ctx.paths().cache_dir));
        anstream::println!(
            "  mounts:       {}",
            Paths::display(&ctx.paths().mounts_dir)
        );
        anstream::println!(
            "  providers:    {}",
            Paths::display(&ctx.paths().providers_dir)
        );
        anstream::println!(
            "  credentials:  {}",
            Paths::display(&ctx.paths().credentials_file)
        );
        anstream::println!(
            "  config file:  {}",
            Paths::display(&ctx.paths().config_file)
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerDescription {
    state: &'static str,
    image: Option<String>,
}

async fn describe_container(container_name: &str) -> ContainerDescription {
    let Ok(docker) = Docker::connect_with_local_defaults() else {
        return ContainerDescription {
            state: "docker unreachable",
            image: None,
        };
    };
    if docker.ping().await.is_err() {
        return ContainerDescription {
            state: "docker unreachable",
            image: None,
        };
    }
    match docker.inspect_container(container_name, None).await {
        Ok(c) => {
            let running = c.state.and_then(|s| s.running).unwrap_or(false);
            let image = c.config.and_then(|config| config.image);
            ContainerDescription {
                state: if running { "running" } else { "stopped" },
                image,
            }
        },
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => ContainerDescription {
            state: "not created",
            image: None,
        },
        Err(_) => ContainerDescription {
            state: "inspect failed",
            image: None,
        },
    }
}

async fn describe_image_location(image: &ImageRef) -> String {
    let origin = image.origin();
    let Ok(docker) = Docker::connect_with_local_defaults() else {
        return format!("{origin}, cache unknown");
    };
    if docker.ping().await.is_err() {
        return format!("{origin}, cache unknown");
    }
    match docker.inspect_image(image.as_str()).await {
        Ok(_) if origin == ImageOrigin::Remote => "remote, cached locally".to_string(),
        Ok(_) => "local".to_string(),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) if origin == ImageOrigin::Remote => "remote, not cached locally".to_string(),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => "local, not built".to_string(),
        Err(_) => format!("{origin}, cache inspect failed"),
    }
}

fn provider_dir_summary(catalog: &ProviderCatalog) -> String {
    match catalog.provider_dir_status() {
        ProviderDirStatus::Missing => "provider dir missing".to_string(),
        ProviderDirStatus::Present { wasm_count } => format!("{wasm_count} on disk"),
        ProviderDirStatus::Unreadable(error) => format!("provider dir unreadable: {error}"),
    }
}
