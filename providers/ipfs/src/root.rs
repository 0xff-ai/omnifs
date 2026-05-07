use std::num::NonZeroU64;

use omnifs_sdk::prelude::*;

use crate::api::{IpfsApi, LsLink};
use crate::types::{CidText, IpnsName};
use crate::{Result, State};

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    #[dir("/")]
    fn root(_cx: &DirCx<'_, State>) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.dir("ipfs");
        projection.dir("ipns");
        projection.dir(".meta");
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/ipfs")]
    async fn ipfs_index(cx: &DirCx<'_, State>) -> Result<Projection> {
        let enumerate = cx.state(|s| s.config.enumerate_pins);
        let mut projection = Projection::new();
        if enumerate {
            for cid in IpfsApi::new(cx).pin_list().await? {
                projection.dir(cid);
            }
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/ipfs/{cid}/{*subpath}")]
    async fn cid_dir(cx: &DirCx<'_, State>, cid: CidText, subpath: String) -> Result<Projection> {
        let upstream = ipfs_path(&cid, &subpath);
        let api = IpfsApi::new(cx);
        let Some(object) = api.try_ls(&upstream).await? else {
            return Err(ProviderError::not_a_directory(format!(
                "{upstream} is not a UnixFS directory"
            )));
        };
        let mut projection = Projection::new();
        for link in object.links {
            emit_link(&mut projection, &link);
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[file("/ipfs/{cid}/{*subpath}")]
    async fn cid_file(cx: &Cx<State>, cid: CidText, subpath: String) -> Result<FileContent> {
        let upstream = ipfs_path(&cid, &subpath);
        let bytes = IpfsApi::new(cx).cat(&upstream).await?;
        Ok(FileContent::bytes(bytes))
    }

    #[dir("/ipns")]
    async fn ipns_index(cx: &DirCx<'_, State>) -> Result<Projection> {
        let enumerate = cx.state(|s| s.config.enumerate_keys);
        let mut projection = Projection::new();
        if enumerate {
            for name in IpfsApi::new(cx).key_list().await? {
                projection.dir(name);
            }
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/ipns/{name}/{*subpath}")]
    async fn ipns_dir(
        cx: &DirCx<'_, State>,
        name: IpnsName,
        subpath: String,
    ) -> Result<Projection> {
        let api = IpfsApi::new(cx);
        let resolved = api.resolve_ipns(&name).await?;
        let target = parse_resolved_ipfs_target(&resolved).ok_or_else(|| {
            ProviderError::not_found(format!("IPNS name {name} did not resolve to /ipfs/..."))
        })?;
        let upstream = ipfs_path(&target.root, &join_subpath(&target.subpath, &subpath));
        let Some(object) = api.try_ls(&upstream).await? else {
            return Err(ProviderError::not_a_directory(format!(
                "{upstream} is not a UnixFS directory"
            )));
        };
        let mut projection = Projection::new();
        for link in object.links {
            emit_link(&mut projection, &link);
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[file("/ipns/{name}/{*subpath}")]
    async fn ipns_file(cx: &Cx<State>, name: IpnsName, subpath: String) -> Result<FileContent> {
        let api = IpfsApi::new(cx);
        let resolved = api.resolve_ipns(&name).await?;
        let target = parse_resolved_ipfs_target(&resolved).ok_or_else(|| {
            ProviderError::not_found(format!("IPNS name {name} did not resolve to /ipfs/..."))
        })?;
        let upstream = ipfs_path(&target.root, &join_subpath(&target.subpath, &subpath));
        let bytes = api.cat(&upstream).await?;
        Ok(FileContent::bytes(bytes))
    }
}

struct ResolvedTarget {
    root: CidText,
    subpath: String,
}

fn parse_resolved_ipfs_target(path: &str) -> Option<ResolvedTarget> {
    let rest = path.strip_prefix("/ipfs/")?;
    let (cid, subpath) = rest
        .split_once('/')
        .map_or((rest, ""), |(cid, subpath)| (cid, subpath));
    Some(ResolvedTarget {
        root: cid.parse().ok()?,
        subpath: subpath.to_string(),
    })
}

fn ipfs_path(root: &CidText, subpath: &str) -> String {
    if subpath.is_empty() {
        format!("/ipfs/{root}")
    } else {
        format!("/ipfs/{root}/{subpath}")
    }
}

fn join_subpath(prefix: &str, suffix: &str) -> String {
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_string(),
        (_, true) => prefix.to_string(),
        _ => format!("{prefix}/{suffix}"),
    }
}

fn emit_link(projection: &mut Projection, link: &LsLink) {
    if link.is_directory() {
        projection.dir(link.name.clone());
    } else {
        projection.file_with_stat(link.name.clone(), nonzero_size(link.size));
    }
}

fn nonzero_size(size: u64) -> FileStat {
    FileStat {
        size: NonZeroU64::new(size).unwrap_or_else(|| NonZeroU64::new(4096).unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resolved_target_extracts_root_and_subpath() {
        let parsed = parse_resolved_ipfs_target(
            "/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/docs/readme.md",
        )
        .unwrap();
        assert!(parsed.root.to_string().starts_with("bafy"));
        assert_eq!(parsed.subpath, "docs/readme.md");
    }

    #[test]
    fn parse_resolved_target_handles_root_only() {
        let parsed = parse_resolved_ipfs_target(
            "/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        )
        .unwrap();
        assert_eq!(parsed.subpath, "");
    }

    #[test]
    fn parse_resolved_target_rejects_non_ipfs_paths() {
        assert!(parse_resolved_ipfs_target("/ipns/example.com").is_none());
    }

    #[test]
    fn join_subpath_avoids_duplicate_separators() {
        assert_eq!(join_subpath("", "docs"), "docs");
        assert_eq!(join_subpath("docs", ""), "docs");
        assert_eq!(join_subpath("docs", "readme.md"), "docs/readme.md");
        assert_eq!(join_subpath("", ""), "");
    }
}
