use crate::cache;
use crate::omnifs::provider::types as wit_types;

#[derive(Clone, Debug)]
pub enum Op {
    LookupChild {
        parent_path: String,
        name: String,
    },
    ListChildren {
        path: String,
    },
    ReadFile {
        path: String,
    },
    OpenFile {
        path: String,
    },
    ReadChunk {
        handle: u64,
        offset: u64,
        length: u32,
    },
    Initialize,
    OnEvent {
        event: wit_types::ProviderEvent,
    },
}

pub(super) struct Validator<'a, F> {
    op: &'a Op,
    ret: &'a wit_types::ProviderReturn,
    eager_bytes: usize,
    tree_exists: F,
}

impl<'a, F> Validator<'a, F>
where
    F: Fn(u64) -> bool,
{
    pub(super) fn returned(
        op: &'a Op,
        ret: &'a wit_types::ProviderReturn,
        tree_exists: F,
    ) -> std::result::Result<(), String> {
        Self {
            op,
            ret,
            eager_bytes: 0,
            tree_exists,
        }
        .validate_return()
    }

    fn validate_return(&mut self) -> std::result::Result<(), String> {
        self.error_returns_do_not_mutate()?;
        self.op_result()?;
        self.effects()?;
        self.subtree_handoff()?;
        Ok(())
    }

    fn error_returns_do_not_mutate(&self) -> std::result::Result<(), String> {
        if matches!(self.ret.result, wit_types::OpResult::Error(_)) && !self.ret.effects.is_empty()
        {
            return Err("provider error returns must not carry effects".to_string());
        }
        Ok(())
    }

    fn effects(&mut self) -> std::result::Result<(), String> {
        for effect in &self.ret.effects {
            match effect {
                wit_types::Effect::Project(entry) => self
                    .entry(&entry.kind)
                    .map_err(|error| format!("project effect {:?}: {error}", entry.path))?,
                wit_types::Effect::InvalidatePath(_) | wit_types::Effect::InvalidatePrefix(_) => {},
                wit_types::Effect::DisownTree(handoff) => {
                    if !(self.tree_exists)(handoff.tree) {
                        return Err(format!(
                            "disown-tree effect for {:?} references unknown tree {}",
                            handoff.path, handoff.tree
                        ));
                    }
                },
            }
        }
        Ok(())
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
                    | wit_types::LookupChildResult::NotFound,
                ),
            )
            | (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(_)),
            )
            | (Op::ReadChunk { .. }, wit_types::OpResult::ReadChunk(_))
            | (Op::Initialize, wit_types::OpResult::Initialize(_))
            | (Op::OnEvent { .. }, wit_types::OpResult::OnEvent)
            | (_, wit_types::OpResult::Error(_)) => {},
            (
                Op::ListChildren { .. },
                wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(listing)),
            ) => {
                for entry in &listing.entries {
                    self.entry(&entry.kind)?;
                }
            },
            (Op::ReadFile { .. }, wit_types::OpResult::ReadFile(result)) => {
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

    fn subtree_handoff(&self) -> std::result::Result<(), String> {
        let handoffs = self.disown_handoffs();
        let subtree = match &self.ret.result {
            wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(tree))
            | wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Subtree(tree)) => {
                Some(*tree)
            },
            _ => None,
        };

        match subtree {
            Some(tree) => {
                if handoffs.len() != 1 || handoffs[0].tree != tree {
                    return Err(format!(
                        "subtree result for tree {tree} requires exactly one matching disown-tree effect"
                    ));
                }
                self.validate_handoff_path(tree, handoffs[0])?;
            },
            None if !handoffs.is_empty() => {
                return Err(format!(
                    "disown-tree effects require a subtree result, got {} orphan effect(s)",
                    handoffs.len()
                ));
            },
            None => {},
        }

        Ok(())
    }

    fn validate_handoff_path(
        &self,
        tree: u64,
        handoff: &wit_types::TreeHandoff,
    ) -> std::result::Result<(), String> {
        match self.op {
            Op::LookupChild { parent_path, name } => {
                let expected = if parent_path.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_path}/{name}")
                };
                if handoff.path != expected {
                    return Err(format!(
                        "subtree result for tree {tree} requires disown-tree path {:?}, got {:?}",
                        expected, handoff.path
                    ));
                }
            },
            Op::ListChildren { path } => {
                if handoff.path != *path {
                    return Err(format!(
                        "subtree result for tree {tree} requires disown-tree path {:?}, got {:?}",
                        path, handoff.path
                    ));
                }
            },
            _ => {
                return Err(format!(
                    "subtree result for tree {tree} is not valid for {:?}",
                    self.op
                ));
            },
        }
        Ok(())
    }

    fn disown_handoffs(&self) -> Vec<&wit_types::TreeHandoff> {
        self.ret
            .effects
            .iter()
            .filter_map(|effect| match effect {
                wit_types::Effect::DisownTree(handoff) => Some(handoff),
                _ => None,
            })
            .collect()
    }

    fn entry(&mut self, kind: &wit_types::EntryKind) -> std::result::Result<(), String> {
        match kind {
            wit_types::EntryKind::Directory => Ok(()),
            wit_types::EntryKind::File(file) => self.file_proj(file),
        }
    }

    fn file_proj(&mut self, file: &wit_types::FileProj) -> std::result::Result<(), String> {
        let attrs = cache::FileAttrsCache::from(file);
        attrs.validate()?;
        self.add_eager_bytes(attrs.eager_byte_len())
    }

    fn read_file_result(
        &mut self,
        result: &wit_types::ReadFileResult,
    ) -> std::result::Result<(), String> {
        Self::file_attrs_metadata(&result.attrs)?;
        match &result.bytes {
            wit_types::ReadFileBytes::Inline(bytes) => {
                let attrs = cache::FileAttrsCache::from(&result.attrs);
                attrs
                    .validate_complete_content(bytes.len())
                    .map_err(|error| format!("read-file result: {error}"))?;
                self.add_eager_bytes(bytes.len())?;
            },
            wit_types::ReadFileBytes::Blob(_) => {},
        }
        Ok(())
    }

    fn file_attrs_metadata(attrs: &wit_types::FileAttrs) -> std::result::Result<(), String> {
        if let Some(token) = &attrs.version_token {
            if token.is_empty() {
                return Err("version token must not be empty".to_string());
            }
            if token.len() > cache::MAX_VERSION_TOKEN_BYTES {
                return Err(format!(
                    "version token exceeds {} bytes",
                    cache::MAX_VERSION_TOKEN_BYTES
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
        if self.eager_bytes > cache::MAX_EAGER_RESPONSE_BYTES {
            return Err(format!(
                "terminal response exceeds aggregate eager byte limit of {} bytes",
                cache::MAX_EAGER_RESPONSE_BYTES
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn validate_operation_result(
    result: &wit_types::OpResult,
) -> std::result::Result<(), String> {
    let op = match result {
        wit_types::OpResult::LookupChild(_) => Op::LookupChild {
            parent_path: String::new(),
            name: "child".to_string(),
        },
        wit_types::OpResult::ListChildren(_) => Op::ListChildren {
            path: String::new(),
        },
        wit_types::OpResult::ReadFile(_) => Op::ReadFile {
            path: "file".to_string(),
        },
        wit_types::OpResult::OpenFile(_) => Op::OpenFile {
            path: "file".to_string(),
        },
        wit_types::OpResult::ReadChunk(_) => Op::ReadChunk {
            handle: 0,
            offset: 0,
            length: 0,
        },
        wit_types::OpResult::Initialize(_) | wit_types::OpResult::Error(_) => Op::Initialize,
        wit_types::OpResult::OnEvent => Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext {
                active_paths: Vec::new(),
            }),
        },
    };
    let ret = wit_types::ProviderReturn {
        result: result.clone(),
        effects: Vec::new(),
    };
    Validator::returned(&op, &ret, |_| true)
}

#[cfg(test)]
pub(super) fn validate_return(
    op: &Op,
    ret: &wit_types::ProviderReturn,
) -> std::result::Result<(), String> {
    Validator::returned(op, ret, |_| true)
}

#[cfg(test)]
mod attr_contract_tests {
    use super::*;

    fn on_event_op() -> Op {
        Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick(wit_types::TimerTickContext {
                active_paths: Vec::new(),
            }),
        }
    }

    fn lookup_op(parent_path: &str, name: &str) -> Op {
        Op::LookupChild {
            parent_path: parent_path.to_string(),
            name: name.to_string(),
        }
    }

    fn attrs(size: wit_types::FileSize, stability: wit_types::Stability) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            stability,
            version_token: None,
        }
    }

    fn file_proj(
        size: wit_types::FileSize,
        bytes: wit_types::ProjBytes,
        stability: wit_types::Stability,
    ) -> wit_types::FileProj {
        wit_types::FileProj {
            attrs: attrs(size, stability),
            bytes,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileProj {
        file_proj(
            wit_types::FileSize::Exact(size),
            wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Immutable,
        )
    }

    #[test]
    fn rejects_invalid_inline_projection_in_entries() {
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "bad".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Inline(b"bad".to_vec()),
                        wit_types::Stability::Immutable,
                    )),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("inline bytes require FileSize::Exact"));
    }

    #[test]
    fn rejects_volatile_non_ranged_attrs() {
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "tail".to_string(),
                    kind: wit_types::EntryKind::File(file_proj(
                        wit_types::FileSize::Unknown,
                        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Volatile,
                    )),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("Stability::Volatile requires"));
    }

    #[test]
    fn rejects_bad_project_effect_size_and_aggregate_eager_cap() {
        let mut bad_size_file = deferred_exact(4);
        bad_size_file.bytes = wit_types::ProjBytes::Inline(b"toolong".to_vec());
        let bad_size = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: vec![wit_types::Effect::Project(wit_types::ProjEntry {
                path: "bad".to_string(),
                kind: wit_types::EntryKind::File(bad_size_file),
                listing_exhaustive: false,
            })],
        };
        let error = validate_return(&on_event_op(), &bad_size).unwrap_err();
        assert!(error.contains("declares size 4"));

        let too_large = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: (0..9)
                .map(|index| {
                    let bytes = vec![0; cache::MAX_INLINE_PROJECTABLE_BYTES];
                    wit_types::Effect::Project(wit_types::ProjEntry {
                        path: format!("large-{index}"),
                        kind: wit_types::EntryKind::File(wit_types::FileProj {
                            attrs: attrs(
                                wit_types::FileSize::Exact(bytes.len() as u64),
                                wit_types::Stability::Immutable,
                            ),
                            bytes: wit_types::ProjBytes::Inline(bytes),
                        }),
                        listing_exhaustive: false,
                    })
                })
                .collect(),
        };
        let error = validate_return(&on_event_op(), &too_large).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let result = wit_types::OpResult::ReadFile(wit_types::ReadFileResult {
            attrs: attrs(
                wit_types::FileSize::NonZero,
                wit_types::Stability::Immutable,
            ),
            bytes: wit_types::ReadFileBytes::Inline(Vec::new()),
        });

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("read-file result"));
        assert!(error.contains("Size::NonZero"));
    }

    #[test]
    fn rejects_empty_version_tokens() {
        let mut file = deferred_exact(1);
        file.attrs.version_token = Some(String::new());
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "versioned".to_string(),
                    kind: wit_types::EntryKind::File(file),
                }],
                exhaustive: true,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("version token must not be empty"));
    }

    #[test]
    fn subtree_results_require_matching_disown_effect() {
        let missing = wit_types::ProviderReturn {
            result: wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(7)),
            effects: Vec::new(),
        };
        let error = validate_return(&lookup_op("", "checkout"), &missing).unwrap_err();
        assert!(error.contains("requires exactly one matching disown-tree effect"));

        let valid = wit_types::ProviderReturn {
            result: wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(7)),
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        validate_return(&lookup_op("", "checkout"), &valid).unwrap();

        let error = validate_return(&lookup_op("", "other"), &valid).unwrap_err();
        assert!(error.contains("requires disown-tree path"));

        let orphan = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: vec![wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: "checkout".to_string(),
                tree: 7,
            })],
        };
        let error = validate_return(&on_event_op(), &orphan).unwrap_err();
        assert!(error.contains("require a subtree result"));
    }

    #[test]
    fn error_returns_reject_effects() {
        let ret = wit_types::ProviderReturn {
            result: wit_types::OpResult::Error(wit_types::ProviderError {
                kind: wit_types::ErrorKind::Internal,
                message: "failed".to_string(),
                retryable: false,
            }),
            effects: vec![wit_types::Effect::InvalidatePath("x".to_string())],
        };

        let error = validate_return(&on_event_op(), &ret).unwrap_err();
        assert!(error.contains("error returns must not carry effects"));
    }
}
