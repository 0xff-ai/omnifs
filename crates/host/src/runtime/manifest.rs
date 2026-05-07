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
}

pub fn read_declared_handlers_from_wasm(path: &Path) -> Result<Vec<DeclaredHandler>, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    let section_bytes = mts::read_manifest_section(&bytes).map_err(|error| error.to_string())?;
    if section_bytes.is_empty() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    for record in mts::ManifestRecordIter::new(&section_bytes) {
        records
            .push(record.map_err(|error| format!("decoding provider manifest record: {error}"))?);
    }
    let resolved = mts::resolve_manifest(records)
        .map_err(|error| format!("resolving provider manifest: {error}"))?;

    // Skip bind sites: they're parents of expanded subtree routes, not
    // concrete handlers the runtime can dispatch.
    resolved
        .handlers
        .into_iter()
        .filter(|handler| !matches!(handler.handler_kind, mts::HandlerKindRecord::Subtree))
        .map(DeclaredHandler::new)
        .collect()
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
