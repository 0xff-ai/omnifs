//! `omnifs config init` and `omnifs config export` handlers.

use crate::{commands::daemon_start, error::ExitCode, rpc::RpcClient, ui::output::Output};
use omnifs_kcl::render_config;
use std::path::PathBuf;

pub fn init(output: &Output) -> anyhow::Result<ExitCode> {
    output.require_human("config init")?;
    let source = "\
# omnifs declarative resources
# KCL is client-side authoring; strict Rust resource types remain authoritative.
config = {
    apiVersion = \"omnifs.dev/v1alpha1\"
    resources = []
}
";
    output.write_raw_bytes(source.as_bytes())?;
    Ok(ExitCode::Success)
}

pub async fn export(output: Output) -> anyhow::Result<ExitCode> {
    output.require_human("config export")?;
    daemon_start::start(&output).await?;
    let snapshot = RpcClient::resolve()?.resources().await?;
    let declarations = omnifs_api::ResourceDeclarations {
        api_version: omnifs_api::API_VERSION.to_owned(),
        resources: snapshot.resources,
    }
    .normalize()?;
    let source = render_config(&declarations);
    output.write_raw_bytes(source.as_bytes())?;
    Ok(ExitCode::Success)
}

pub(crate) fn default_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let path = PathBuf::from("omnifs.k");
    anyhow::ensure!(
        path.is_file(),
        "no omnifs.k in the current directory; pass a path"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::default_path;
    use std::path::PathBuf;

    #[test]
    fn explicit_path_is_preserved() {
        let path = PathBuf::from("some/omnifs.k");
        assert_eq!(default_path(Some(path.clone())).unwrap(), path);
    }
}
