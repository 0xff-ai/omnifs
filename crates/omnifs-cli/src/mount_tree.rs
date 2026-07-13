//! `omnifs mount-tree` subcommand implementation.
//!
//! Reads the `omnifs.provider-manifest.v1` custom section from a provider
//! wasm file and renders views of the declared path handlers.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use omnifs_workspace::provider as mts;

#[allow(clippy::struct_excessive_bools)]
pub struct Views {
    pub tree: bool,
    pub paths: bool,
    pub by_type: bool,
}

impl Views {
    pub fn with_defaults(self) -> Self {
        if self.tree || self.paths || self.by_type {
            self
        } else {
            Self {
                tree: true,
                paths: true,
                by_type: false,
            }
        }
    }
}

pub struct MountTreeData {
    pub handlers: Vec<mts::HandlerRecord>,
    pub mutations: Vec<mts::MutationRecord>,
}

impl MountTreeData {
    pub fn read_from_wasm(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let wasm = mts::ProviderWasm::from_bytes(bytes);

        let section_bytes = wasm
            .manifest_section()
            .context("reading provider-manifest section")?;
        if section_bytes.is_empty() {
            bail!(
                "no {} custom section found in {}",
                mts::MANIFEST_SECTION_NAME,
                path.display()
            );
        }

        let records = wasm
            .manifest_records()
            .context("decoding provider manifest records")?
            .into_iter()
            .filter_map(|record| match record {
                mts::ManifestRecord::Unknown { tag, .. } => {
                    crate::ui::eprint_raw(&format!(
                        "warning: unknown provider-manifest tag 0x{tag:02x}, skipping\n"
                    ));
                    None
                },
                other => Some(other),
            })
            .collect();

        let resolved =
            mts::ResolvedManifest::resolve(records).context("resolving provider manifest")?;

        if resolved.handlers.is_empty() && resolved.mutations.is_empty() {
            bail!(
                "no handler or mutation records in {} custom section of {}",
                mts::MANIFEST_SECTION_NAME,
                path.display()
            );
        }

        Ok(Self {
            handlers: resolved.handlers,
            mutations: resolved.mutations,
        })
    }

    pub fn render(&self, views: Views) -> String {
        let views = views.with_defaults();
        let sections = [
            views.tree.then(|| self.render_tree()),
            views.paths.then(|| self.render_paths()),
            views.by_type.then(|| self.render_by_type()),
            (!self.mutations.is_empty()).then(|| self.render_mutations()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        let mut out = sections.join("\n");
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn sorted_handlers(&self) -> Vec<&mts::HandlerRecord> {
        let mut handlers: Vec<&mts::HandlerRecord> = self.handlers.iter().collect();
        handlers.sort_by(|left, right| left.path_template.cmp(&right.path_template));
        handlers
    }

    fn render_tree(&self) -> String {
        let handlers = self.sorted_handlers();

        let mut body = String::new();
        for handler in &handlers {
            let indent = "  ".repeat(path_depth(&handler.path_template));
            let _ = writeln!(
                body,
                "{indent}{} -> {} [{}]",
                path_tail(&handler.path_template),
                handler.handler_name,
                handler_kind_label(&handler.handler_kind),
            );
        }

        format!("{}{}", section_header("Tree"), body)
    }

    fn render_paths(&self) -> String {
        let handlers = self.sorted_handlers();

        let col_width = handlers
            .iter()
            .map(|handler| handler.path_template.len())
            .max()
            .unwrap_or(0)
            + 2;

        let mut body = String::new();
        for handler in &handlers {
            let right = format!(
                "{} [{}]",
                handler.handler_name,
                handler_kind_label(&handler.handler_kind),
            );
            let _ = writeln!(body, "{:<col_width$}{right}", handler.path_template);
        }

        format!("{}{}", section_header("Paths"), body)
    }

    fn render_by_type(&self) -> String {
        let mut groups: HashMap<&str, Vec<&mts::HandlerRecord>> = HashMap::new();
        for handler in &self.handlers {
            groups
                .entry(&handler.handler_name)
                .or_default()
                .push(handler);
        }

        let mut groups = groups.into_iter().collect::<Vec<_>>();
        groups.sort_by(|left, right| left.0.cmp(right.0));

        let col_width = groups.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 2;

        let mut body = String::new();
        for (name, handlers) in groups {
            let mut handlers = handlers;
            handlers.sort_by(|left, right| left.path_template.cmp(&right.path_template));

            let first = handlers[0];
            let first_right = format!(
                "{} [{}]",
                first.path_template,
                handler_kind_label(&first.handler_kind),
            );
            let _ = writeln!(body, "{name:<col_width$}{first_right}");

            for handler in handlers.iter().skip(1) {
                let right = format!(
                    "{} [{}]",
                    handler.path_template,
                    handler_kind_label(&handler.handler_kind),
                );
                let _ = writeln!(body, "{:<col_width$}{right}", "");
            }
        }

        format!("{}{}", section_header("By type"), body)
    }

    fn render_mutations(&self) -> String {
        let mut mutations: Vec<&mts::MutationRecord> = self.mutations.iter().collect();
        mutations.sort_by(|left, right| left.path_template.cmp(&right.path_template));

        let mut body = String::new();
        for mutation in &mutations {
            let _ = writeln!(body, "{}", mutation.path_template);
        }

        format!("{}{}", section_header("Mutations"), body)
    }
}

fn section_header(name: &str) -> String {
    format!("{name}\n{}\n", "=".repeat(60))
}

fn handler_kind_label(kind: &mts::HandlerKindRecord) -> &'static str {
    match kind {
        mts::HandlerKindRecord::Dir => "dir",
        mts::HandlerKindRecord::File => "file",
        mts::HandlerKindRecord::TreeRef => "treeref",
        mts::HandlerKindRecord::Subtree => "subtree",
    }
}

fn path_depth(path: &str) -> usize {
    if path == "/" {
        0
    } else {
        path.chars().filter(|&c| c == '/').count()
    }
}

fn path_tail(path: &str) -> &str {
    if path == "/" {
        "/"
    } else {
        path.rsplit('/').next().unwrap_or(path)
    }
}
