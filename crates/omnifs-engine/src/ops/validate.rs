//! Validates provider returns against the invoked [`Op`](super::op::Op).

use std::collections::HashMap;

use super::op::Op;
use crate::object_id::ObjectId;
use crate::view::{MAX_EAGER_RESPONSE_BYTES, MAX_VERSION_TOKEN_BYTES};
use crate::wit_protocol::{try_file_attrs_from_attrs, try_file_attrs_from_file_out};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

pub(crate) fn validate_return<F>(
    op: &Op,
    ret: &wit_types::ProviderReturn,
    tree_exists: F,
) -> std::result::Result<(), String>
where
    F: Fn(u64) -> bool,
{
    ReturnValidator {
        op,
        ret,
        eager_bytes: 0,
        tree_exists,
    }
    .validate()
}

struct ReturnValidator<'a, F> {
    op: &'a Op,
    ret: &'a wit_types::ProviderReturn,
    eager_bytes: usize,
    tree_exists: F,
}

impl<F> ReturnValidator<'_, F>
where
    F: Fn(u64) -> bool,
{
    fn validate(&mut self) -> std::result::Result<(), String> {
        self.error_returns_do_not_mutate()?;
        self.op_result()?;
        self.effects()?;
        self.subtree_tree()?;
        Ok(())
    }

    fn error_returns_do_not_mutate(&self) -> std::result::Result<(), String> {
        if matches!(self.ret.result, wit_types::OpResult::Error(_))
            && !effects_empty(&self.ret.effects)
        {
            return Err("provider error returns must not carry effects".to_string());
        }
        Ok(())
    }

    fn effects(&mut self) -> std::result::Result<(), String> {
        let effects = &self.ret.effects;
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
                if omnifs_core::path::Path::parse(leaf).is_err() {
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
                    .map_err(|error| format!("fs-write {:?}: {error}", write.path))?;
            }
        }

        for invalidation in &effects.invalidations {
            match invalidation {
                wit_types::Invalidation::Object(_) => {},
                wit_types::Invalidation::Listing(
                    wit_types::PathOrPrefix::Path(p) | wit_types::PathOrPrefix::Prefix(p),
                ) => {
                    if omnifs_core::path::Path::parse(p).is_err() {
                        return Err(format!(
                            "invalidation path {p:?} is not a valid protocol path"
                        ));
                    }
                },
            }
        }

        Ok(())
    }

    fn track_unique_path_id(
        path_to_id: &mut HashMap<String, Vec<u8>>,
        path: &str,
        id_bytes: &[u8],
    ) -> std::result::Result<(), String> {
        match path_to_id.get(path) {
            Some(other) if other.as_slice() != id_bytes => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            Some(_) => Err(format!("duplicate path {path:?} in one return")),
            None => {
                path_to_id.insert(path.to_string(), id_bytes.to_vec());
                Ok(())
            },
        }
    }

    fn check_path_id_conflict(
        path_to_id: &HashMap<String, Vec<u8>>,
        path: &str,
        id_bytes: &[u8],
    ) -> std::result::Result<(), String> {
        match path_to_id.get(path) {
            Some(other) if other.as_slice() != id_bytes => Err(format!(
                "path {path:?} maps to two different object ids in one return"
            )),
            _ => Ok(()),
        }
    }

    fn op_result(&mut self) -> std::result::Result<(), String> {
        match (self.op, &self.ret.result) {
            (
                Op::LookupChild { .. },
                wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Entry(entry)),
            ) => {
                self.entry(&entry.target.kind)?;
                for sibling in &entry.siblings {
                    self.entry(&sibling.kind)?;
                }
            },
            (
                Op::LookupChild { .. },
                wit_types::OpResult::LookupChild(
                    wit_types::LookupChildResult::Subtree(_)
                    | wit_types::LookupChildResult::NotFound(_),
                ),
            )
            | (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(_)),
            )
            | (Op::ReadChunk { .. }, wit_types::OpResult::ReadChunk(_))
            | (Op::Initialize, wit_types::OpResult::Initialize(_))
            | (Op::OnEvent { .. }, wit_types::OpResult::OnEvent)
            | (_, wit_types::OpResult::Error(_))
            | (
                Op::ReadFile { .. },
                wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::NotFound(_)),
            ) => {},
            (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(listing)),
            ) => {
                for entry in &listing.entries {
                    self.entry(&entry.kind)?;
                }
            },
            (
                Op::ReadFile { .. },
                wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::Found(result)),
            ) => {
                self.read_file_result(result)?;
            },
            (Op::OpenFile { .. }, wit_types::OpResult::OpenFile(result)) => {
                Self::file_attrs_metadata(&result.attrs)?;
            },
            _ => {
                return Err(format!(
                    "{:?} returned unexpected result: {:?}",
                    self.op, self.ret.result
                ));
            },
        }
        Ok(())
    }

    fn subtree_tree(&self) -> std::result::Result<(), String> {
        let tree = match &self.ret.result {
            wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(tree))
            | wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(tree)) => {
                *tree
            },
            _ => return Ok(()),
        };
        if !(self.tree_exists)(tree) {
            return Err(format!("subtree result references unknown tree {tree}"));
        }
        Ok(())
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
                    .map_err(|error| format!("read-file result: {error}"))?;
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

fn effects_empty(effects: &wit_types::Effects) -> bool {
    effects.canonical.is_empty() && effects.fs.is_empty() && effects.invalidations.is_empty()
}
