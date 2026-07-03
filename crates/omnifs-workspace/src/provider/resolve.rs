use crate::provider::records::{
    HandlerKindRecord, HandlerRecord, ManifestRecord, MutationRecord, SubtreeRouteRecord,
};

/// The flat list of routes a manifest declares, with subtree binds
/// expanded into one absolute `HandlerRecord` per (bind, subtree-route)
/// pair. Bind sites themselves remain as records with
/// `handler_kind == Subtree` and `subtree_type` set.
#[derive(Clone, Debug, Default)]
pub struct ResolvedManifest {
    pub handlers: Vec<HandlerRecord>,
    pub mutations: Vec<MutationRecord>,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ResolveError {
    /// A bind site references a `subtree_type` for which no
    /// `SubtreeRouteRecord`s were emitted in the manifest.
    #[error(
        "bind site {path_template} references subtree type {subtree_type:?} but no SubtreeRouteRecord matches; ensure the `#[subtree] impl` names the same syntactic type as the bind's return type"
    )]
    UnresolvedBind {
        path_template: String,
        subtree_type: String,
    },
    /// A bind record was emitted with `handler_kind == Subtree` but
    /// without a `subtree_type` set.
    #[error("bind site {path_template} has handler_kind=Subtree but no subtree_type")]
    BindMissingSubtreeType { path_template: String },
    /// A `SubtreeRouteRecord` was emitted but no bind site references
    /// its `subtree_type`. Likely a partially-stripped binary or a
    /// stale subtree impl.
    #[error("subtree route {path_template} (type {subtree_type:?}) has no bind site")]
    OrphanedSubtreeRoute {
        subtree_type: String,
        path_template: String,
    },
}

/// Resolve raw manifest records into a flat list of absolute handler
/// records. Bind sites are paired with subtree routes by `subtree_type`;
/// each pair produces one absolute `HandlerRecord` with the templates
/// concatenated (bind + relative) and capture schemas appended in that
/// order.
pub fn resolve_manifest(records: Vec<ManifestRecord>) -> Result<ResolvedManifest, ResolveError> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut handlers: Vec<HandlerRecord> = Vec::new();
    let mut mutations: Vec<MutationRecord> = Vec::new();
    let mut binds: Vec<HandlerRecord> = Vec::new();
    let mut subtree_routes: BTreeMap<String, Vec<SubtreeRouteRecord>> = BTreeMap::new();

    for record in records {
        match record {
            ManifestRecord::Handler(handler) => {
                if handler.handler_kind == HandlerKindRecord::Subtree {
                    binds.push(handler.clone());
                }
                handlers.push(handler);
            },
            ManifestRecord::Mutation(mutation) => mutations.push(mutation),
            ManifestRecord::SubtreeRoute(route) => {
                subtree_routes
                    .entry(route.subtree_type.clone())
                    .or_default()
                    .push(route);
            },
            ManifestRecord::Unknown { .. } => {},
        }
    }

    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for bind in binds {
        let Some(subtree_type) = bind.subtree_type.as_deref() else {
            return Err(ResolveError::BindMissingSubtreeType {
                path_template: bind.path_template,
            });
        };
        let Some(routes) = subtree_routes.get(subtree_type) else {
            return Err(ResolveError::UnresolvedBind {
                path_template: bind.path_template,
                subtree_type: subtree_type.to_string(),
            });
        };
        referenced.insert(subtree_type.to_string());

        for route in routes {
            let mut capture_schema = bind.capture_schema.clone();
            capture_schema.extend(route.capture_schema.iter().cloned());
            handlers.push(HandlerRecord {
                path_template: join_template(&bind.path_template, &route.path_template),
                handler_name: route.handler_name.clone(),
                handler_kind: route.handler_kind.clone(),
                capture_schema,
                subtree_type: None,
            });
        }
    }

    if let Some((subtree_type, mut routes)) = subtree_routes
        .into_iter()
        .find(|(ty, _)| !referenced.contains(ty))
    {
        let representative = routes.remove(0);
        return Err(ResolveError::OrphanedSubtreeRoute {
            subtree_type,
            path_template: representative.path_template,
        });
    }

    Ok(ResolvedManifest {
        handlers,
        mutations,
    })
}

/// Concatenate a bind path template with a subtree-relative template.
/// Both are absolute (start with `/`); the relative template's leading
/// `/` is consumed when the bind path is non-root, and a relative
/// template of `/` collapses to the bind path itself.
pub(crate) fn join_template(bind: &str, relative: &str) -> String {
    if relative == "/" {
        return bind.to_string();
    }
    if bind == "/" {
        return relative.to_string();
    }
    format!("{bind}{relative}")
}

#[cfg(test)]
mod tests {
    use super::resolve_manifest;
    use crate::provider::records::{
        HandlerKindRecord, HandlerRecord, ManifestCaptureRecord, ManifestRecord, SubtreeRouteRecord,
    };
    use crate::provider::resolve::ResolveError;

    fn cap(name: &str, ty: &str) -> ManifestCaptureRecord {
        ManifestCaptureRecord {
            name: name.to_string(),
            type_name: ty.to_string(),
        }
    }

    fn bind(path: &str, captures: Vec<ManifestCaptureRecord>) -> HandlerRecord {
        HandlerRecord {
            path_template: path.to_string(),
            handler_name: "BindSite".to_string(),
            handler_kind: HandlerKindRecord::Subtree,
            capture_schema: captures,
            subtree_type: Some("PaperSubtree".to_string()),
        }
    }

    fn route(
        path: &str,
        kind: HandlerKindRecord,
        captures: Vec<ManifestCaptureRecord>,
    ) -> SubtreeRouteRecord {
        SubtreeRouteRecord {
            subtree_type: "PaperSubtree".to_string(),
            path_template: path.to_string(),
            handler_name: "Route".to_string(),
            handler_kind: kind,
            capture_schema: captures,
        }
    }

    #[test]
    fn resolve_manifest_pairs_binds_with_subtree_routes() {
        let records = vec![
            ManifestRecord::Handler(bind("/papers/{paper}", vec![cap("paper", "PaperKey")])),
            ManifestRecord::Handler(bind(
                "/categories/{category}/{year}/{month}/{paper}",
                vec![
                    cap("category", "CategoryKey"),
                    cap("year", "u32"),
                    cap("month", "u32"),
                    cap("paper", "PaperKey"),
                ],
            )),
            ManifestRecord::SubtreeRoute(route("/", HandlerKindRecord::Dir, Vec::new())),
            ManifestRecord::SubtreeRoute(route("/paper.pdf", HandlerKindRecord::File, Vec::new())),
            ManifestRecord::SubtreeRoute(route(
                "/versions/{version}/paper.pdf",
                HandlerKindRecord::File,
                vec![cap("version", "VersionKey")],
            )),
        ];

        let resolved = resolve_manifest(records).unwrap();
        assert_eq!(resolved.handlers.len(), 8);

        let templates: Vec<&str> = resolved
            .handlers
            .iter()
            .map(|h| h.path_template.as_str())
            .collect();
        assert!(templates.contains(&"/papers/{paper}"));
        assert!(templates.contains(&"/papers/{paper}/paper.pdf"));
        assert!(templates.contains(&"/papers/{paper}/versions/{version}/paper.pdf"));
        assert!(templates.contains(&"/categories/{category}/{year}/{month}/{paper}"));
        assert!(templates.contains(&"/categories/{category}/{year}/{month}/{paper}/paper.pdf"));
        assert!(templates.contains(
            &"/categories/{category}/{year}/{month}/{paper}/versions/{version}/paper.pdf"
        ));

        let merged = resolved
            .handlers
            .iter()
            .find(|h| {
                h.path_template
                    == "/categories/{category}/{year}/{month}/{paper}/versions/{version}/paper.pdf"
            })
            .unwrap();
        let names: Vec<&str> = merged
            .capture_schema
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["category", "year", "month", "paper", "version"]);
    }

    #[test]
    fn resolve_manifest_root_bind_collapses_relative_root() {
        let records = vec![
            ManifestRecord::Handler(bind("/", Vec::new())),
            ManifestRecord::SubtreeRoute(route("/", HandlerKindRecord::Dir, Vec::new())),
            ManifestRecord::SubtreeRoute(route("/inner", HandlerKindRecord::File, Vec::new())),
        ];
        let resolved = resolve_manifest(records).unwrap();
        let templates: Vec<&str> = resolved
            .handlers
            .iter()
            .map(|h| h.path_template.as_str())
            .collect();
        assert!(templates.contains(&"/"));
        assert!(templates.contains(&"/inner"));
    }

    #[test]
    fn resolve_manifest_unresolved_bind_errors() {
        let records = vec![ManifestRecord::Handler(bind("/papers/{paper}", Vec::new()))];
        let error = resolve_manifest(records).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::UnresolvedBind { ref subtree_type, .. } if subtree_type == "PaperSubtree"
        ));
    }

    #[test]
    fn resolve_manifest_orphaned_subtree_route_errors() {
        let records = vec![ManifestRecord::SubtreeRoute(route(
            "/",
            HandlerKindRecord::Dir,
            Vec::new(),
        ))];
        let error = resolve_manifest(records).unwrap_err();
        assert!(matches!(error, ResolveError::OrphanedSubtreeRoute { .. }));
    }

    #[test]
    fn resolve_manifest_handles_multiple_binds_to_same_subtree_type() {
        let records = vec![
            ManifestRecord::Handler(bind("/papers/{paper}", vec![cap("paper", "PaperKey")])),
            ManifestRecord::Handler(bind(
                "/authors/{author}/{paper}",
                vec![cap("author", "AuthorKey"), cap("paper", "PaperKey")],
            )),
            ManifestRecord::SubtreeRoute(route("/", HandlerKindRecord::Dir, Vec::new())),
            ManifestRecord::SubtreeRoute(route("/paper.pdf", HandlerKindRecord::File, Vec::new())),
        ];
        let resolved = resolve_manifest(records).unwrap();
        let templates: Vec<&str> = resolved
            .handlers
            .iter()
            .map(|h| h.path_template.as_str())
            .collect();
        assert!(templates.contains(&"/papers/{paper}/paper.pdf"));
        assert!(templates.contains(&"/authors/{author}/{paper}/paper.pdf"));
    }
}
