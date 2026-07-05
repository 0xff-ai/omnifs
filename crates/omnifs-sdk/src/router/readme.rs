//! Generated provider README leaves.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::descriptor::{RouteDescriptor, RouteKind};
use super::pattern::Pattern;

pub(super) const README_FILE: &str = "README.md";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Root,
    Branch(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ObjectLeaves {
    pub template: String,
    pub leaf_names: Vec<String>,
}

pub(super) struct Readme<'a> {
    scope: Scope,
    routes: &'a [RouteDescriptor],
    object_leaves: &'a [ObjectLeaves],
}

impl Scope {
    pub fn root_path() -> String {
        format!("/{README_FILE}")
    }

    pub fn readme_path(&self) -> String {
        match self {
            Self::Root => Self::root_path(),
            Self::Branch(branch) => format!("/{branch}/{README_FILE}"),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Root => "/".to_string(),
            Self::Branch(branch) => format!("/{branch}"),
        }
    }

    fn contains_template(&self, template: &str) -> bool {
        match self {
            Self::Root => true,
            Self::Branch(branch) => {
                let prefix = format!("/{branch}/");
                template == format!("/{branch}") || template.starts_with(&prefix)
            },
        }
    }

    fn example_path(&self, template: &str) -> String {
        let scoped = match self {
            Self::Root => template,
            Self::Branch(branch) => template
                .strip_prefix(&format!("/{branch}"))
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .unwrap_or(template),
        };
        match scoped {
            "" | "/" => ".".to_string(),
            path if path.starts_with('/') => format!(".{path}"),
            path => format!("./{path}"),
        }
    }
}

impl<'a> Readme<'a> {
    pub fn new(
        scope: Scope,
        routes: &'a [RouteDescriptor],
        object_leaves: &'a [ObjectLeaves],
    ) -> Self {
        Self {
            scope,
            routes,
            object_leaves,
        }
    }

    pub fn render(&self) -> String {
        let routes = self.scoped_routes();
        let mut out = String::new();
        out.push_str("# Omnifs route schema\n\n");
        let _ = writeln!(
            out,
            "This README is generated from the sealed provider route table for `{}`.\n",
            self.scope.label()
        );
        out.push_str("## Keying schema\n\n");
        out.push_str(
            "The keying schema is the path grammar below. Literal segments are written as-is. Captures such as `{owner}` are parsed by the provider SDK. A finite choice list means only those path values are valid. Lookup may resolve capture values that `ls` cannot enumerate.\n\n",
        );
        out.push_str("## Route templates\n\n");
        if routes.is_empty() {
            out.push_str("- No provider routes are declared under this scope.\n");
        } else {
            for route in &routes {
                let _ = writeln!(
                    out,
                    "- `{}` - {}",
                    route.template,
                    route_kind_description(route)
                );
                for capture in &route.captures {
                    let _ = writeln!(
                        out,
                        "  - `{{{}}}`: `{}`{}",
                        capture.name,
                        capture.type_name,
                        choices_suffix(capture.choices.as_deref())
                    );
                }
            }
        }
        out.push('\n');
        out.push_str("## Example commands\n\n");
        for command in self.examples(&routes) {
            let _ = writeln!(out, "- `{command}`");
        }
        out.push('\n');
        out.push_str("## Bulk traversal\n\n");
        out.push_str(
            "Mount-root ignore files hide generated README leaves and pagination controls from ignore-respecting recursive tools. Read this file explicitly with `cat README.md` when you need the schema.\n",
        );
        out
    }

    fn scoped_routes(&self) -> Vec<&'a RouteDescriptor> {
        self.routes
            .iter()
            .filter(|route| self.scope.contains_template(&route.template))
            .collect()
    }

    fn examples(&self, routes: &[&RouteDescriptor]) -> Vec<String> {
        let mut commands = vec!["ls .".to_string()];
        if let Some(template) = routes
            .iter()
            .find(|route| is_browsable(route.kind))
            .map(|route| route.template.as_str())
        {
            commands.push(format!(
                "ls {}",
                shell_quote(&self.scope.example_path(template))
            ));
        }
        if let Some(path) = self.read_example_path(routes) {
            commands.push(format!("cat {}", shell_quote(&path)));
        }
        commands.push(format!("cat ./{README_FILE}"));
        commands.truncate(3);
        commands
    }

    fn read_example_path(&self, routes: &[&RouteDescriptor]) -> Option<String> {
        routes
            .iter()
            .find(|route| route.kind == RouteKind::File || route.kind == RouteKind::FileObject)
            .map(|route| self.scope.example_path(&route.template))
            .or_else(|| self.object_leaf_example())
    }

    fn object_leaf_example(&self) -> Option<String> {
        self.object_leaves
            .iter()
            .find(|object| {
                self.scope.contains_template(&object.template) && !object.leaf_names.is_empty()
            })
            .map(|object| {
                let path = format!(
                    "{}/{}",
                    object.template.trim_end_matches('/'),
                    object.leaf_names[0]
                );
                self.scope.example_path(&path)
            })
    }
}

pub(super) fn branch_scopes(routes: &[RouteDescriptor]) -> Vec<Scope> {
    let branches = routes
        .iter()
        .filter_map(top_level_readme_branch)
        .collect::<BTreeSet<_>>();
    branches.into_iter().map(Scope::Branch).collect()
}

fn top_level_readme_branch(route: &RouteDescriptor) -> Option<String> {
    let pattern = Pattern::parse(&route.template).ok()?;
    let branch = pattern.first_literal_segment()?;
    if branch == README_FILE {
        return None;
    }
    if pattern.pattern_len() > 1
        || matches!(
            route.kind,
            RouteKind::Dir | RouteKind::Object | RouteKind::Collection
        )
    {
        return Some(branch.to_string());
    }
    None
}

fn is_browsable(kind: RouteKind) -> bool {
    matches!(
        kind,
        RouteKind::Dir
            | RouteKind::Treeref
            | RouteKind::Object
            | RouteKind::Alias
            | RouteKind::Collection
    )
}

fn route_kind_description(route: &RouteDescriptor) -> String {
    match (route.kind, route.object_kind.as_deref()) {
        (RouteKind::Dir, _) => "directory".to_string(),
        (RouteKind::File, _) => "file".to_string(),
        (RouteKind::Treeref, _) => "subtree".to_string(),
        (RouteKind::Object, Some(kind)) => format!("object `{kind}`"),
        (RouteKind::FileObject, Some(kind)) => format!("file object `{kind}`"),
        (RouteKind::Alias, Some(kind)) => format!("alias for `{kind}`"),
        (RouteKind::Collection, Some(kind)) => format!("collection of `{kind}`"),
        (RouteKind::Object, None) => "object".to_string(),
        (RouteKind::FileObject, None) => "file object".to_string(),
        (RouteKind::Alias, None) => "alias".to_string(),
        (RouteKind::Collection, None) => "collection".to_string(),
    }
}

fn choices_suffix(choices: Option<&[String]>) -> String {
    let Some(choices) = choices else {
        return String::new();
    };
    if choices.is_empty() {
        return String::new();
    }
    format!(
        " choices {}",
        choices
            .iter()
            .map(|choice| format!("`{choice}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn shell_quote(path: &str) -> String {
    if path == "." {
        return ".".to_string();
    }
    format!("'{}'", path.replace('\'', "'\\''"))
}
