use omnifs_sdk::prelude::*;

use crate::api::IpfsApi;
use crate::types::CidText;
use crate::{Result, State};

pub struct MetaHandlers;

#[handlers]
impl MetaHandlers {
    #[dir("/.meta")]
    fn meta_index(_cx: &DirCx<'_, State>) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/.meta/{cid}")]
    async fn meta_dir(cx: &DirCx<'_, State>, cid: CidText) -> Result<Projection> {
        let summary = inspect_cid(cx, &cid).await?;
        let mut projection = Projection::new();
        projection.file_with_content("cid", cid.to_string().into_bytes());
        projection.file_with_content("kind", summary.kind_label().as_bytes().to_vec());
        projection.file_with_content("codec", codec_name(&cid).as_bytes().to_vec());
        projection.file_with_content("block_size", summary.block_size.to_string().into_bytes());
        projection.file_with_content("dag_size", summary.dag_size.to_string().into_bytes());
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[file("/.meta/{cid}/cid")]
    fn meta_cid(_cx: &Cx<State>, cid: CidText) -> Result<FileContent> {
        Ok(FileContent::bytes(cid.to_string().into_bytes()))
    }

    #[file("/.meta/{cid}/kind")]
    async fn meta_kind(cx: &Cx<State>, cid: CidText) -> Result<FileContent> {
        let summary = inspect_cid(cx, &cid).await?;
        Ok(FileContent::bytes(summary.kind_label().as_bytes().to_vec()))
    }

    #[file("/.meta/{cid}/codec")]
    fn meta_codec(_cx: &Cx<State>, cid: CidText) -> Result<FileContent> {
        Ok(FileContent::bytes(codec_name(&cid).as_bytes().to_vec()))
    }

    #[file("/.meta/{cid}/block_size")]
    async fn meta_block_size(cx: &Cx<State>, cid: CidText) -> Result<FileContent> {
        let summary = inspect_cid(cx, &cid).await?;
        Ok(FileContent::bytes(
            summary.block_size.to_string().into_bytes(),
        ))
    }

    #[file("/.meta/{cid}/dag_size")]
    async fn meta_dag_size(cx: &Cx<State>, cid: CidText) -> Result<FileContent> {
        let summary = inspect_cid(cx, &cid).await?;
        Ok(FileContent::bytes(
            summary.dag_size.to_string().into_bytes(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootKind {
    Directory,
    File,
    Raw,
    Dag,
}

struct CidSummary {
    block_size: u64,
    dag_size: u64,
    kind: RootKind,
}

impl CidSummary {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            RootKind::Directory => "directory",
            RootKind::File => "file",
            RootKind::Raw => "raw",
            RootKind::Dag => "dag",
        }
    }
}

async fn inspect_cid(cx: &Cx<State>, cid: &CidText) -> Result<CidSummary> {
    let api = IpfsApi::new(cx);
    let (block_stat, dag_stat) = api.block_and_dag_stat(cid).await?;
    let kind = classify_root(cid, &api).await?;
    Ok(CidSummary {
        block_size: block_stat.size,
        dag_size: dag_stat.total_size(),
        kind,
    })
}

async fn classify_root(cid: &CidText, api: &IpfsApi<'_>) -> Result<RootKind> {
    if cid.codec() == 0x55 {
        return Ok(RootKind::Raw);
    }
    let root_path = format!("/ipfs/{cid}");
    if api.probe_cat(&root_path).await?.is_some() {
        return Ok(RootKind::File);
    }
    if api.try_ls(&root_path).await?.is_some() {
        return Ok(RootKind::Directory);
    }
    Ok(RootKind::Dag)
}

fn codec_name(cid: &CidText) -> &'static str {
    match cid.codec() {
        0x55 => "raw",
        0x70 => "dag-pb",
        0x71 => "dag-cbor",
        0x72 => "libp2p-key",
        _ => "unknown",
    }
}
