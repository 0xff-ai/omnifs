use omnifs_core::path::{Path, Segment};
use omnifs_wit::provider::types as wit_types;

#[derive(Clone, Debug)]
pub enum Op {
    LookupChild {
        parent_path: Path,
        name: Segment,
    },
    ListChildren {
        path: Path,
        /// Listing validator the host holds for this path (OPEN-8), echoed
        /// so the provider can answer `unchanged` (ADR-0001 §8).
        cached_validator: Option<String>,
        /// Resume token for a paged listing; `None` for a plain readdir.
        cursor: Option<wit_types::Cursor>,
    },
    ReadFile {
        path: Path,
        /// Content type the host echoes opaquely into `read-file`
        /// (ADR-0001 §5.1).
        content_type: String,
        /// Canonical bytes the host pushes for this path's anchor on a
        /// View-cache miss, so the SDK renders without an upstream call.
        cached_canonical: Option<wit_types::CanonicalInput>,
    },
    OpenFile {
        path: Path,
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

/// Descriptor used by the live observability stream. `None` for ops that
/// do not produce a user-observable `provider.start`/`provider.end` pair.
pub struct LiveOpDescriptor {
    pub method: &'static str,
    pub path: Path,
}

impl Op {
    /// Return the descriptor when this op should appear in the live stream.
    pub fn live_descriptor(&self) -> Option<LiveOpDescriptor> {
        let (method, path) = match self {
            Self::LookupChild { parent_path, name } => {
                ("lookup_child", parent_path.join_segment(name))
            },
            Self::ListChildren { path, .. } => ("list_children", path.clone()),
            Self::ReadFile { path, .. } => ("read_file", path.clone()),
            Self::OpenFile { .. }
            | Self::ReadChunk { .. }
            | Self::Initialize
            | Self::OnEvent { .. } => return None,
        };
        Some(LiveOpDescriptor { method, path })
    }
}

#[cfg(test)]
pub(super) fn validate_operation_result(
    result: &wit_types::OpResult,
) -> std::result::Result<(), String> {
    let op = match result {
        wit_types::OpResult::LookupChild(_) => Op::LookupChild {
            parent_path: Path::root(),
            name: Segment::try_from("child").unwrap(),
        },
        wit_types::OpResult::ListChildren(_) => Op::ListChildren {
            path: Path::root(),
            cached_validator: None,
            cursor: None,
        },
        wit_types::OpResult::ReadFile(
            wit_types::ReadFileOutcome::Found(_) | wit_types::ReadFileOutcome::NotFound(_),
        ) => Op::ReadFile {
            path: Path::from_validated("/file"),
            content_type: "application/octet-stream".to_string(),
            cached_canonical: None,
        },
        wit_types::OpResult::OpenFile(_) => Op::OpenFile {
            path: Path::from_validated("/file"),
        },
        wit_types::OpResult::ReadChunk(_) => Op::ReadChunk {
            handle: 0,
            offset: 0,
            length: 0,
        },
        wit_types::OpResult::Initialize(_) | wit_types::OpResult::Error(_) => Op::Initialize,
        wit_types::OpResult::OnEvent => Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick,
        },
    };
    let ret = wit_types::ProviderReturn {
        result: result.clone(),
        effects: empty_effects(),
    };
    super::op_validate::validate_return(&op, &ret, |_| true)
}

#[cfg(test)]
fn empty_effects() -> wit_types::Effects {
    wit_types::Effects {
        canonical: Vec::new(),
        fs: Vec::new(),
        invalidations: Vec::new(),
    }
}

#[cfg(test)]
pub(super) fn validate_return(
    op: &Op,
    ret: &wit_types::ProviderReturn,
) -> std::result::Result<(), String> {
    super::op_validate::validate_return(op, ret, |_| true)
}

#[cfg(test)]
mod attr_contract_tests {
    use super::*;
    use omnifs_core::view::MAX_INLINE_PROJECTABLE_BYTES;

    fn on_event_op() -> Op {
        Op::OnEvent {
            event: wit_types::ProviderEvent::TimerTick,
        }
    }

    fn lookup_op(parent_path: &str, name: &str) -> Op {
        Op::LookupChild {
            parent_path: Path::parse(parent_path).expect("test parent path"),
            name: Segment::try_from(name).unwrap(),
        }
    }

    fn attrs(size: wit_types::FileSize, stability: wit_types::Stability) -> wit_types::FileAttrs {
        wit_types::FileAttrs {
            size,
            stability,
            version_token: None,
        }
    }

    fn file_out(
        size: wit_types::FileSize,
        bytes: wit_types::ByteSource,
        stability: wit_types::Stability,
    ) -> wit_types::FileOut {
        wit_types::FileOut {
            attrs: attrs(size, stability),
            bytes,
            content_type: None,
        }
    }

    fn deferred_exact(size: u64) -> wit_types::FileOut {
        file_out(
            wit_types::FileSize::Exact(size),
            wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
            wit_types::Stability::Stable,
        )
    }

    #[test]
    fn rejects_invalid_inline_projection_in_entries() {
        let result = wit_types::OpResult::ListChildren(wit_types::ListChildrenResult::Entries(
            wit_types::DirListing {
                entries: vec![wit_types::DirEntry {
                    name: "bad".to_string(),
                    id: None,
                    kind: wit_types::EntryKind::File(file_out(
                        wit_types::FileSize::Unknown,
                        wit_types::ByteSource::Inline(b"bad".to_vec()),
                        wit_types::Stability::Stable,
                    )),
                }],
                exhaustive: true,
                validator: None,
                next_cursor: None,
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
                    id: None,
                    kind: wit_types::EntryKind::File(file_out(
                        wit_types::FileSize::Unknown,
                        wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
                        wit_types::Stability::Live,
                    )),
                }],
                exhaustive: true,
                validator: None,
                next_cursor: None,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("Stability::Live requires"));
    }

    fn fs_file_write(path: String, file: wit_types::FileOut) -> wit_types::FsWrite {
        wit_types::FsWrite {
            id: None,
            path,
            kind: wit_types::FsKind::File(file),
        }
    }

    #[test]
    fn rejects_invalid_fs_write_path_without_id() {
        let ret = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: wit_types::Effects {
                fs: vec![fs_file_write("bad".to_string(), deferred_exact(1))],
                ..empty_effects()
            },
        };

        let error = validate_return(&on_event_op(), &ret).unwrap_err();
        assert!(error.contains("fs-write path"));
        assert!(error.contains("valid protocol path"));
    }

    #[test]
    fn rejects_bad_fs_write_size_and_aggregate_eager_cap() {
        let mut bad_size_file = deferred_exact(4);
        bad_size_file.bytes = wit_types::ByteSource::Inline(b"toolong".to_vec());
        let bad_size = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: wit_types::Effects {
                fs: vec![fs_file_write("/bad".to_string(), bad_size_file)],
                ..empty_effects()
            },
        };
        let error = validate_return(&on_event_op(), &bad_size).unwrap_err();
        assert!(error.contains("declares size 4"));

        let too_large = wit_types::ProviderReturn {
            result: wit_types::OpResult::OnEvent,
            effects: wit_types::Effects {
                fs: (0..9)
                    .map(|index| {
                        let bytes = vec![0; MAX_INLINE_PROJECTABLE_BYTES];
                        fs_file_write(
                            format!("/large-{index}"),
                            wit_types::FileOut {
                                attrs: attrs(
                                    wit_types::FileSize::Exact(bytes.len() as u64),
                                    wit_types::Stability::Stable,
                                ),
                                bytes: wit_types::ByteSource::Inline(bytes),
                                content_type: None,
                            },
                        )
                    })
                    .collect(),
                ..empty_effects()
            },
        };
        let error = validate_return(&on_event_op(), &too_large).unwrap_err();
        assert!(error.contains("aggregate eager byte limit"));
    }

    #[test]
    fn rejects_read_content_that_violates_declared_size() {
        let result = wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::Found(
            wit_types::ReadFileResult {
                content_type: None,
                attrs: attrs(wit_types::FileSize::NonZero, wit_types::Stability::Stable),
                bytes: wit_types::ByteSource::Inline(Vec::new()),
            },
        ));

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
                    id: None,
                    kind: wit_types::EntryKind::File(file),
                }],
                exhaustive: true,
                validator: None,
                next_cursor: None,
            },
        ));

        let error = validate_operation_result(&result).unwrap_err();
        assert!(error.contains("version token must not be empty"));
    }

    #[test]
    fn subtree_result_requires_known_tree() {
        let unknown_tree = wit_types::ProviderReturn {
            result: wit_types::OpResult::LookupChild(wit_types::LookupChildResult::Subtree(7)),
            effects: empty_effects(),
        };
        let error =
            crate::op_validate::validate_return(&lookup_op("/", "checkout"), &unknown_tree, |_| {
                false
            })
            .unwrap_err();
        assert!(error.contains("references unknown tree 7"));

        crate::op_validate::validate_return(&lookup_op("/", "checkout"), &unknown_tree, |_| true)
            .unwrap();
    }

    #[test]
    fn error_returns_reject_effects() {
        let ret = wit_types::ProviderReturn {
            result: wit_types::OpResult::Error(wit_types::ProviderError {
                kind: wit_types::ErrorKind::Internal,
                message: "failed".to_string(),
                retryable: false,
                retry_after: None,
            }),
            effects: wit_types::Effects {
                invalidations: vec![wit_types::Invalidation::Listing(
                    wit_types::PathOrPrefix::Path("x".to_string()),
                )],
                ..empty_effects()
            },
        };

        let error = validate_return(&on_event_op(), &ret).unwrap_err();
        assert!(error.contains("error returns must not carry effects"));
    }
}
