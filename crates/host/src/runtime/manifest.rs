use std::path::Path;

use omnifs_mount_schema as mts;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredHandler {
    pub mount_id: String,
    pub mount_name: String,
    pub kind: DeclaredHandlerKind,
    pattern: mts::PathPattern,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredHandlerKind {
    Dir,
    File,
    TreeRef,
    Subtree,
}

impl DeclaredHandler {
    fn new(record: mts::HandlerRecord) -> Result<Self, String> {
        let pattern = mts::PathPattern::parse(&record.path_template)
            .map_err(|error| error.message().to_string())?;
        let kind = match record.handler_kind {
            mts::HandlerKindRecord::Dir => DeclaredHandlerKind::Dir,
            mts::HandlerKindRecord::File => DeclaredHandlerKind::File,
            mts::HandlerKindRecord::TreeRef => DeclaredHandlerKind::TreeRef,
            mts::HandlerKindRecord::Subtree => DeclaredHandlerKind::Subtree,
        };
        Ok(Self {
            mount_id: record.path_template,
            mount_name: record.handler_name,
            kind,
            pattern,
        })
    }

    pub fn concrete_path_for(&self, concrete_path: &str) -> Option<String> {
        self.pattern.concrete_path_for(concrete_path)
    }

    pub fn matches_exact_path(&self, concrete_path: &str) -> bool {
        self.pattern.matches_exact_path(concrete_path)
    }

    pub fn pattern_len(&self) -> usize {
        self.pattern.pattern_len()
    }

    pub fn specificity(&self) -> &[(u8, usize)] {
        self.pattern.specificity()
    }

    pub fn resolve_touched(
        handlers: &[DeclaredHandler],
        absolute: &str,
    ) -> Vec<crate::runtime::activity::ActivePathTouch> {
        let mut best_by_depth = std::collections::BTreeMap::new();
        for mount in handlers {
            let Some(concrete_path) = mount.concrete_path_for(absolute) else {
                continue;
            };
            match best_by_depth.entry(mount.pattern_len()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((mount, concrete_path));
                },
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let current = slot.get().0;
                    if mount
                        .specificity()
                        .iter()
                        .cmp(current.specificity().iter())
                        .is_gt()
                    {
                        slot.insert((mount, concrete_path));
                    }
                },
            }
        }
        best_by_depth
            .into_values()
            .map(
                |(mount, concrete_path)| crate::runtime::activity::ActivePathTouch {
                    mount_id: mount.mount_id.clone(),
                    mount_name: mount.mount_name.clone(),
                    path: concrete_path,
                },
            )
            .collect()
    }
}

fn load_provider_wasm(path: &Path) -> Result<mts::ProviderWasm, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    Ok(mts::ProviderWasm::from_bytes(bytes))
}

pub fn read_declared_handlers_from_wasm(path: &Path) -> Result<Vec<DeclaredHandler>, String> {
    let wasm = load_provider_wasm(path)?;
    if wasm
        .manifest_section()
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Ok(Vec::new());
    }

    let resolved = wasm.resolved_manifest().map_err(|error| match error {
        mts::ProviderWasmError::Decode(decode) => {
            format!("decoding provider manifest record: {decode}")
        },
        mts::ProviderWasmError::Resolve(resolve) => {
            format!("resolving provider manifest: {resolve}")
        },
        mts::ProviderWasmError::Section(section) => section.to_string(),
    })?;

    // Skip bind sites: they're parents of expanded subtree routes, not
    // concrete handlers the runtime can dispatch.
    resolved
        .handlers
        .into_iter()
        .filter(|handler| !matches!(handler.handler_kind, mts::HandlerKindRecord::Subtree))
        .map(DeclaredHandler::new)
        .collect()
}

pub fn read_auth_manifest_from_wasm(path: &Path) -> Result<Option<mts::AuthManifest>, String> {
    Ok(read_provider_metadata_from_wasm(path)?.and_then(|manifest| manifest.wasm_auth_manifest()))
}

pub fn read_provider_metadata_from_wasm(
    path: &Path,
) -> Result<Option<mts::ProviderManifest>, String> {
    load_provider_wasm(path)?
        .metadata()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{DeclaredHandler, DeclaredHandlerKind};
    use omnifs_mount_schema::{HandlerKindRecord, HandlerRecord};

    #[test]
    fn declared_handler_matches_capture_patterns_and_returns_concrete_path() {
        let repo = DeclaredHandler::new(HandlerRecord {
            path_template: "/{owner}/{repo}".to_string(),
            handler_name: "Repo".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();
        let issue = DeclaredHandler::new(HandlerRecord {
            path_template: "/{owner}/{repo}/_issues/_open/{number}".to_string(),
            handler_name: "Issue".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();
        let resolver = DeclaredHandler::new(HandlerRecord {
            path_template: "/@{resolver}/{segment}".to_string(),
            handler_name: "ResolverSegment".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();

        assert_eq!(
            repo.concrete_path_for("/openai/gvfs/_issues/_open/7"),
            Some("/openai/gvfs".to_string())
        );
        assert_eq!(
            issue.concrete_path_for("/openai/gvfs/_issues/_open/7/comments/1"),
            Some("/openai/gvfs/_issues/_open/7".to_string())
        );
        assert_eq!(
            resolver.concrete_path_for("/@google/example.com"),
            Some("/@google/example.com".to_string())
        );
        assert_eq!(repo.concrete_path_for("/_resolvers"), None);
        assert_eq!(resolver.concrete_path_for("/@"), None);
    }

    #[test]
    fn declared_handler_specificity_prefers_literals_over_captures() {
        let literal = DeclaredHandler::new(HandlerRecord {
            path_template: "/_resolvers".to_string(),
            handler_name: "Resolvers".to_string(),
            handler_kind: HandlerKindRecord::File,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();
        let prefixed = DeclaredHandler::new(HandlerRecord {
            path_template: "/@{resolver}".to_string(),
            handler_name: "ResolverRoot".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();
        let capture = DeclaredHandler::new(HandlerRecord {
            path_template: "/{segment}".to_string(),
            handler_name: "Segment".to_string(),
            handler_kind: HandlerKindRecord::Dir,
            capture_schema: Vec::new(),
            subtree_type: None,
        })
        .unwrap();

        assert_eq!(literal.kind, DeclaredHandlerKind::File);
        assert!(literal.specificity() > capture.specificity());
        assert!(prefixed.specificity() > capture.specificity());
    }
}
