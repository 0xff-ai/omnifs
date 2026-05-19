//! `/meta` files: library version, configured path, and rich
//! header info.

use omnifs_sdk::prelude::*;

use crate::{Result, State};

pub struct MetaHandlers;

#[handlers]
impl MetaHandlers {
    #[file("/meta/version.txt")]
    fn version(cx: &Cx<State>) -> Result<FileContent> {
        let (bytes, version) = cx.state(|state| {
            let backend = state.backend.borrow();
            let mut bytes = backend.library_version().as_bytes().to_vec();
            bytes.push(b'\n');
            let version = backend.meta_version().ok();
            (bytes, version)
        });
        Ok(file_with_meta_version(bytes, version))
    }

    #[file("/meta/path.txt")]
    fn path(cx: &Cx<State>) -> Result<FileContent> {
        let (bytes, version) = cx.state(|state| {
            let backend = state.backend.borrow();
            let mut bytes = backend.path.clone().into_bytes();
            bytes.push(b'\n');
            let version = backend.meta_version().ok();
            (bytes, version)
        });
        Ok(file_with_meta_version(bytes, version))
    }

    #[file("/meta/info.json")]
    fn info(cx: &Cx<State>) -> Result<FileContent> {
        let (bytes, version) = cx.state(|state| {
            let backend = state.backend.borrow();
            let info = backend
                .file_info()
                .map_err(|e| ProviderError::internal(format!("file_info: {e}")))?;
            let mut bytes = serde_json::to_vec_pretty(&info)
                .map_err(|e| ProviderError::internal(format!("encode info: {e}")))?;
            bytes.push(b'\n');
            let version = backend.meta_version().ok();
            Ok::<_, ProviderError>((bytes, version))
        })?;
        Ok(file_with_meta_version(bytes, version))
    }
}

fn file_with_meta_version(bytes: Vec<u8>, version: Option<String>) -> FileContent {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut attrs = FileAttrs::new(Size::Exact(size), Stability::Mutable);
    if let Some(v) = version {
        attrs = attrs.with_version(v);
    }
    FileContent::bytes_with_attrs(attrs, bytes)
}
