//! Typed validation for provider operation payloads and terminal effects.

use std::collections::HashMap;

use crate::object_id::ObjectId;
use crate::view::{MAX_EAGER_RESPONSE_BYTES, MAX_VERSION_TOKEN_BYTES};
use crate::wit_protocol::{try_file_attrs_from_attrs, try_file_attrs_from_file_out};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

pub(crate) struct ReturnValidator<'a, F> {
    effects: &'a wit_types::Effects,
    eager_bytes: usize,
    tree_exists: F,
}

impl<'a, F> ReturnValidator<'a, F>
where
    F: Fn(u64) -> bool,
{
    pub(crate) fn new(effects: &'a wit_types::Effects, tree_exists: F) -> Self {
        Self {
            effects,
            eager_bytes: 0,
            tree_exists,
        }
    }

    pub(crate) fn common<T>(
        result: &std::result::Result<T, wit_types::ProviderError>,
        effects: &'a wit_types::Effects,
        tree_exists: F,
    ) -> std::result::Result<Self, String> {
        if result.is_err() && !effects_empty(effects) {
            return Err("provider error returns must not carry effects".to_string());
        }
        let mut validator = Self::new(effects, tree_exists);
        validator.effects()?;
        Ok(validator)
    }

    pub(crate) fn lookup(
        &mut self,
        result: &std::result::Result<wit_types::LookupChildResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::LookupChildResult::Entry(entry) => {
                self.entry(&entry.target.kind)?;
                for sibling in &entry.siblings {
                    self.entry(&sibling.kind)?;
                }
            },
            wit_types::LookupChildResult::Subtree(tree) => self.subtree_tree(*tree)?,
            wit_types::LookupChildResult::NotFound(_) => {},
        }
        Ok(())
    }

    pub(crate) fn list(
        &mut self,
        result: &std::result::Result<wit_types::ListChildrenResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::ListChildrenResult::Entries(listing) => {
                for entry in &listing.entries {
                    self.entry(&entry.kind)?;
                }
            },
            wit_types::ListChildrenResult::Subtree(tree) => self.subtree_tree(*tree)?,
            wit_types::ListChildrenResult::Unchanged => {},
        }
        Ok(())
    }

    pub(crate) fn read(
        &mut self,
        result: &std::result::Result<wit_types::ReadFileOutcome, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        let Ok(result) = result else { return Ok(()) };
        match result {
            wit_types::ReadFileOutcome::Found(result) => self.read_file_result(result),
            wit_types::ReadFileOutcome::NotFound(_) => Ok(()),
        }
    }

    pub(crate) fn open(
        &mut self,
        result: &std::result::Result<wit_types::OpenFileResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        if let Ok(result) = result {
            Self::file_attrs_metadata(&result.attrs)?;
        }
        Ok(())
    }

    pub(crate) fn chunk(
        &mut self,
        _result: &std::result::Result<wit_types::ReadChunkResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    pub(crate) fn initialize(
        &mut self,
        _result: &std::result::Result<wit_types::InitializeResult, wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    pub(crate) fn event(
        &mut self,
        _result: &std::result::Result<(), wit_types::ProviderError>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    fn effects(&mut self) -> std::result::Result<(), String> {
        let effects = self.effects;
        let mut canonical_path_to_id: HashMap<String, Vec<u8>> = HashMap::new();
        let mut fs_path_to_id: HashMap<String, Vec<u8>> = HashMap::new();
        for store in &effects.canonical {
            if store.id.kind.is_empty() {
                return Err("canonical-store id.kind must not be empty".to_string());
            }
            if store.view_leaves.is_empty() {
                return Err("canonical-store has an empty view-leaves list".to_string());
            }
            let id_bytes = ObjectId::from_wit(&store.id).as_bytes().to_vec();
            for leaf in &store.view_leaves {
                if leaf.is_empty() {
                    return Err("canonical-store has an empty view-leaf".to_string());
                }
                if Path::parse(leaf).is_err() {
                    return Err(format!(
                        "canonical-store view-leaf {leaf:?} is not a valid protocol path"
                    ));
                }
                Self::track_unique_path_id(&mut canonical_path_to_id, leaf, &id_bytes)?;
            }
            if let Some(token) = &store.validator
                && token.len() > MAX_VERSION_TOKEN_BYTES
            {
                return Err(format!(
                    "canonical-store validator token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
                ));
            }
        }
        for write in &effects.fs {
            if Path::parse(&write.path).is_err() {
                return Err(format!(
                    "fs-write path {:?} is not a valid protocol path",
                    write.path
                ));
            }
            if let Some(id) = &write.id {
                let id_bytes = ObjectId::from_wit(id).as_bytes().to_vec();
                Self::track_unique_path_id(&mut fs_path_to_id, &write.path, &id_bytes)?;
                Self::check_path_id_conflict(&canonical_path_to_id, &write.path, &id_bytes)?;
            }
            if let wit_types::FsKind::File(file) = &write.kind {
                self.file_out(file)
                    .map_err(|e| format!("fs-write {:?}: {e}", write.path))?;
            }
        }
        for invalidation in &effects.invalidations {
            if let wit_types::Invalidation::Listing(
                wit_types::PathOrPrefix::Path(p) | wit_types::PathOrPrefix::Prefix(p),
            ) = invalidation
                && Path::parse(p).is_err()
            {
                return Err(format!(
                    "invalidation path {p:?} is not a valid protocol path"
                ));
            }
        }
        Ok(())
    }

    fn subtree_tree(&self, tree: u64) -> std::result::Result<(), String> {
        if !(self.tree_exists)(tree) {
            return Err(format!("subtree result references unknown tree {tree}"));
        }
        Ok(())
    }

    fn track_unique_path_id(
        map: &mut HashMap<String, Vec<u8>>,
        path: &str,
        id: &[u8],
    ) -> std::result::Result<(), String> {
        match map.get(path) {
            Some(other) if other.as_slice() != id => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            Some(_) => Err(format!("duplicate path {path:?} in one return")),
            None => {
                map.insert(path.to_string(), id.to_vec());
                Ok(())
            },
        }
    }

    fn check_path_id_conflict(
        map: &HashMap<String, Vec<u8>>,
        path: &str,
        id: &[u8],
    ) -> std::result::Result<(), String> {
        match map.get(path) {
            Some(other) if other.as_slice() != id => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            _ => Ok(()),
        }
    }

    fn entry(&mut self, kind: &wit_types::EntryKind) -> std::result::Result<(), String> {
        match kind {
            wit_types::EntryKind::Directory => Ok(()),
            wit_types::EntryKind::File(file) => self.file_out(file),
        }
    }

    fn file_out(&mut self, file: &wit_types::FileOut) -> std::result::Result<(), String> {
        let attrs = try_file_attrs_from_file_out(file)?;
        attrs.validate()?;
        self.add_eager_bytes(attrs.eager_byte_len())
    }

    fn read_file_result(
        &mut self,
        result: &wit_types::ReadFileResult,
    ) -> std::result::Result<(), String> {
        Self::file_attrs_metadata(&result.attrs)?;
        match &result.bytes {
            wit_types::ByteSource::Inline(bytes) => {
                let attrs = try_file_attrs_from_attrs(&result.attrs)?;
                attrs
                    .validate_complete_content(bytes.len())
                    .map_err(|e| format!("read-file result: {e}"))?;
                self.add_eager_bytes(bytes.len())?;
            },
            wit_types::ByteSource::Canonical | wit_types::ByteSource::Blob(_) => {},
            wit_types::ByteSource::Deferred(_) => {
                return Err(
                    "read-file result: ByteSource::Deferred is not a valid read answer".to_string(),
                );
            },
        }
        Ok(())
    }

    fn file_attrs_metadata(attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        if let Some(token) = &attrs.version_token {
            if token.is_empty() {
                return Err("version token must not be empty".to_string());
            }
            if token.len() > MAX_VERSION_TOKEN_BYTES {
                return Err(format!(
                    "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
                ));
            }
        }
        Ok(())
    }

    fn add_eager_bytes(&mut self, bytes: usize) -> std::result::Result<(), String> {
        self.eager_bytes = self
            .eager_bytes
            .checked_add(bytes)
            .ok_or_else(|| "aggregate eager byte count overflowed".to_string())?;
        if self.eager_bytes > MAX_EAGER_RESPONSE_BYTES {
            return Err(format!(
                "terminal response exceeds aggregate eager byte limit of {MAX_EAGER_RESPONSE_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_lookup<F>(
    result: &std::result::Result<wit_types::LookupChildResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.lookup(result)
}
pub(crate) fn validate_list<F>(
    result: &std::result::Result<wit_types::ListChildrenResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.list(result)
}
pub(crate) fn validate_read<F>(
    result: &std::result::Result<wit_types::ReadFileOutcome, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.read(result)
}
pub(crate) fn validate_open<F>(
    result: &std::result::Result<wit_types::OpenFileResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.open(result)
}
pub(crate) fn validate_chunk<F>(
    result: &std::result::Result<wit_types::ReadChunkResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.chunk(result)
}
pub(crate) fn validate_initialize<F>(
    result: &std::result::Result<wit_types::InitializeResult, wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.initialize(result)
}
pub(crate) fn validate_event<F>(
    result: &std::result::Result<(), wit_types::ProviderError>,
    effects: &wit_types::Effects,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    let mut v = ReturnValidator::common(result, effects, tree_exists)?;
    v.event(result)
}

fn effects_empty(effects: &wit_types::Effects) -> bool {
    effects.canonical.is_empty() && effects.fs.is_empty() && effects.invalidations.is_empty()
}
