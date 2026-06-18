//! `omnifs provider` — provider introspection.
//!
//! Today the only subcommand is `info`, a man-page-style dump of everything
//! omnifs knows about a provider: its static manifest (read host-side off the
//! WASM custom section, no execution) plus its route table (read by running the
//! provider through the host runtime and calling `initialize`). The assembled
//! [`ProviderMetadata`] model is the reusable unit other surfaces (doc-page
//! generation) consume; rendering lives entirely at this CLI use site.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use clap::{Args, Subcommand};
use omnifs_host::{Instance, component_engine};
use omnifs_provider::{
    AuthScheme, CapabilityEntry, OAuthFlow, ProviderAuthManifest, ProviderManifest,
    read_provider_metadata_section,
};
use omnifs_wit::provider::types as wit;
use serde::Serialize;

use crate::app_context::AppContext;
use crate::capability::{capability_label, capability_value};
use crate::presentation::OutputFormat;
use crate::style;

#[derive(Args, Debug)]
pub struct ProviderArgs {
    #[command(subcommand)]
    command: ProviderCommand,
}

#[derive(Subcommand, Debug)]
enum ProviderCommand {
    /// Show everything omnifs knows about a provider: branding, mount,
    /// contract, capabilities, auth, and its full route tree.
    ///
    /// `<PROVIDER>` is a provider name (its id, default mount, or wasm file
    /// stem, resolved against the configured providers directory) or an
    /// explicit path to a provider `.wasm`.
    Info(InfoArgs),
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Provider name or path to a provider `.wasm`.
    provider: String,

    /// Directory to resolve a provider name against. Defaults to the configured
    /// providers directory. Ignored when `<PROVIDER>` is a path.
    #[arg(long = "providers-dir")]
    providers_dir: Option<PathBuf>,

    /// Emit the assembled metadata as JSON instead of the man-page rendering.
    #[arg(long = "json")]
    json: bool,
}

impl ProviderArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ProviderCommand::Info(args) => args.run(),
        }
    }
}

impl InfoArgs {
    fn run(self) -> anyhow::Result<()> {
        let wasm_path = self.resolve_wasm_path()?;
        let metadata = assemble_provider_metadata(&wasm_path)?;
        match OutputFormat::from(self.json) {
            OutputFormat::Json => {
                anstream::println!("{}", serde_json::to_string_pretty(&metadata)?);
            },
            OutputFormat::Text => render_man_page(&metadata),
        }
        Ok(())
    }

    /// Resolve `<PROVIDER>` to a provider `.wasm` on disk. An argument that looks
    /// like a path (contains a separator or a `.wasm` suffix) is used directly;
    /// otherwise it is treated as a name and matched against the providers dir.
    fn resolve_wasm_path(&self) -> anyhow::Result<PathBuf> {
        let arg = self.provider.as_str();
        if looks_like_path(arg) {
            let path = PathBuf::from(arg);
            if !path.is_file() {
                anyhow::bail!("provider wasm `{}` does not exist", path.display());
            }
            return Ok(path);
        }

        let providers_dir = match &self.providers_dir {
            Some(dir) => dir.clone(),
            None => AppContext::resolve_default()?.paths().providers_dir.clone(),
        };
        resolve_provider_name(arg, &providers_dir)
    }
}

/// True when the argument should be treated as an explicit filesystem path
/// rather than a provider name to resolve.
fn looks_like_path(arg: &str) -> bool {
    arg.contains(std::path::MAIN_SEPARATOR)
        || arg.contains('/')
        || Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wasm"))
}

/// Match a provider name against the `.wasm` files in `providers_dir`. A match
/// is any of: the manifest `id`, the manifest `defaultMount`, or the wasm file
/// stem. Providers without a readable metadata section are skipped silently;
/// their wasm stem can still match.
fn resolve_provider_name(name: &str, providers_dir: &Path) -> anyhow::Result<PathBuf> {
    let read = std::fs::read_dir(providers_dir).with_context(|| {
        format!(
            "read providers directory {} (pass --providers-dir or a path to a .wasm)",
            providers_dir.display()
        )
    })?;

    let mut candidates = Vec::new();
    for entry in read {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if stem == name {
            return Ok(path);
        }
        if let Some(manifest) = read_manifest_quiet(&path)
            && (manifest.id == name || manifest.default_mount == name)
        {
            return Ok(path);
        }
        candidates.push(stem);
    }

    candidates.sort();
    Err(anyhow!(
        "no provider named `{name}` in {} (available: {})",
        providers_dir.display(),
        if candidates.is_empty() {
            "none".to_string()
        } else {
            candidates.join(", ")
        }
    ))
}

/// Read a provider's embedded manifest, swallowing errors (used during name
/// resolution where an unreadable sibling must not abort the search).
fn read_manifest_quiet(path: &Path) -> Option<ProviderManifest> {
    let bytes = std::fs::read(path).ok()?;
    read_provider_metadata_section(&bytes).ok().flatten()
}

// ---------------------------------------------------------------------------
// Metadata model — the reusable assembly. Other surfaces (doc-page generation)
// consume `assemble_provider_metadata`; rendering is a CLI concern below.
// ---------------------------------------------------------------------------

/// Everything omnifs knows about a single provider, gathered from its `.wasm`:
/// the static manifest (host-side, no execution) plus its route table
/// (introspected by running `initialize`). The route table is honest about the
/// config-gated case via [`RouteIntrospection`].
#[derive(Debug, Serialize)]
pub(crate) struct ProviderMetadata {
    pub manifest: ProviderManifest,
    /// `name`/`version`/`description` from `provider-info`, when `initialize`
    /// succeeded. `None` when the provider is config-gated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<ProviderInfo>,
    pub routes: RouteIntrospection,
}

/// Identity fields read off `provider-info` after a successful `initialize`.
#[derive(Debug, Serialize)]
pub(crate) struct ProviderInfo {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// The outcome of trying to read a provider's route table.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum RouteIntrospection {
    /// Routes read live from `initialize`, rebuilt into a nested tree.
    Introspected { routes: Vec<RouteNode> },
    /// `initialize` failed for empty config and the committed
    /// `omnifs.routes.json` supplied the tree instead.
    FromArtifact { routes: Vec<RouteNode> },
    /// Neither path worked: the provider needs configuration to start and no
    /// committed artifact was found. Rendered honestly rather than crashing.
    RequiresConfig { reason: String },
}

impl RouteIntrospection {
    fn routes(&self) -> &[RouteNode] {
        match self {
            Self::Introspected { routes } | Self::FromArtifact { routes } => routes,
            Self::RequiresConfig { .. } => &[],
        }
    }
}

/// A node in the nested route tree. Mirrors the SDK `RouteDescriptor` JSON shape
/// so the committed `omnifs.routes.json` deserializes straight into it.
#[derive(Debug, Serialize, serde::Deserialize)]
pub(crate) struct RouteNode {
    pub template: String,
    pub kind: RouteKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub representations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<RouteNode>,
}

#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RouteKind {
    Dir,
    File,
    TreeRef,
    Object,
    FileObject,
}

impl RouteKind {
    fn label(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::TreeRef => "tree-ref",
            Self::Object => "object",
            Self::FileObject => "file-object",
        }
    }
}

impl From<wit::RouteKind> for RouteKind {
    fn from(kind: wit::RouteKind) -> Self {
        match kind {
            wit::RouteKind::Dir => Self::Dir,
            wit::RouteKind::File => Self::File,
            wit::RouteKind::TreeRef => Self::TreeRef,
            wit::RouteKind::Object => Self::Object,
            wit::RouteKind::FileObject => Self::FileObject,
        }
    }
}

/// Assemble the full metadata model for a provider `.wasm`.
///
/// The manifest is read host-side from the WASM custom section (no execution).
/// The route table is read by running the provider through the host runtime and
/// calling `initialize` with an empty config — the same path the build-time
/// `introspect_routes` bin uses. A provider whose `start` requires config (the
/// `db` provider needs a `path`) cannot introspect under empty config; it falls
/// back to the committed `providers/<name>/omnifs.routes.json` when present, and
/// otherwise reports [`RouteIntrospection::RequiresConfig`] rather than failing.
pub(crate) fn assemble_provider_metadata(wasm_path: &Path) -> anyhow::Result<ProviderMetadata> {
    let bytes = std::fs::read(wasm_path)
        .with_context(|| format!("read provider wasm {}", wasm_path.display()))?;
    let manifest = read_provider_metadata_section(&bytes)
        .with_context(|| format!("read provider metadata from {}", wasm_path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "{} carries no omnifs provider metadata section",
                wasm_path.display()
            )
        })?;

    match introspect_routes(wasm_path) {
        Ok((info, routes)) => Ok(ProviderMetadata {
            manifest,
            info: Some(info),
            routes: RouteIntrospection::Introspected { routes },
        }),
        Err(IntrospectError::ConfigGated(reason)) => {
            // Config-gated: fall back to a committed routes artifact if we can
            // find one next to a source checkout, else report honestly.
            let routes = match committed_routes_for(wasm_path) {
                Some(routes) => RouteIntrospection::FromArtifact { routes },
                None => RouteIntrospection::RequiresConfig { reason },
            };
            Ok(ProviderMetadata {
                manifest,
                info: None,
                routes,
            })
        },
        Err(IntrospectError::Other(error)) => Err(error),
    }
}

enum IntrospectError {
    /// `initialize` rejected the empty config: provider needs configuration.
    ConfigGated(String),
    Other(anyhow::Error),
}

/// Run the provider and read `provider-info` (identity + routes). Mirrors the
/// build-time `introspect_routes` bin: empty config, single `initialize` call,
/// flattened route list rebuilt into a tree.
fn introspect_routes(wasm_path: &Path) -> Result<(ProviderInfo, Vec<RouteNode>), IntrospectError> {
    let engine = component_engine(|_| {})
        .map_err(|e| IntrospectError::Other(anyhow!("component engine: {e}")))?;
    let instance = Instance::new(&engine, wasm_path, b"{}".to_vec(), &[])
        .map_err(|e| IntrospectError::Other(anyhow!("instantiate provider: {e}")))?;
    let ret = instance
        .initialize()
        .map_err(|e| IntrospectError::Other(anyhow!("initialize provider: {e}")))?;

    let info = match ret.result {
        wit::OpResult::Initialize(result) => result.info,
        wit::OpResult::Error(error) if matches!(error.kind, wit::ErrorKind::InvalidInput) => {
            return Err(IntrospectError::ConfigGated(error.message));
        },
        other => {
            return Err(IntrospectError::Other(anyhow!(
                "initialize returned unexpected result: {other:?}"
            )));
        },
    };

    let routes = rebuild_tree(&info.routes);
    Ok((
        ProviderInfo {
            name: info.name,
            version: info.version,
            description: info.description,
        },
        routes,
    ))
}

/// Locate and parse a committed `omnifs.routes.json` for the provider.
///
/// Best-effort: walks up from the wasm path looking for a workspace `providers/`
/// directory, then matches the artifact by the provider's wasm stem or its
/// declared `defaultMount`/`id`. Returns `None` if nothing is found.
fn committed_routes_for(wasm_path: &Path) -> Option<Vec<RouteNode>> {
    let manifest = read_manifest_quiet(wasm_path);
    let stem = wasm_path.file_stem().and_then(|s| s.to_str());

    // Candidate provider-subdirectory names to try under a `providers/` root.
    let mut names: Vec<String> = Vec::new();
    if let Some(manifest) = &manifest {
        names.push(manifest.default_mount.clone());
        names.push(manifest.id.clone());
    }
    if let Some(stem) = stem {
        names.push(stem.to_string());
    }

    let providers_root = find_providers_root(wasm_path)?;
    for name in names {
        let candidate = providers_root.join(&name).join("omnifs.routes.json");
        if let Some(routes) = read_routes_artifact(&candidate) {
            return Some(routes);
        }
    }
    None
}

/// Walk upward from `start` looking for a sibling `providers/` directory that
/// holds per-provider source subdirectories.
fn find_providers_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.parent();
    while let Some(current) = dir {
        let candidate = current.join("providers");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

fn read_routes_artifact(path: &Path) -> Option<Vec<RouteNode>> {
    #[derive(serde::Deserialize)]
    struct Artifact {
        routes: Vec<RouteNode>,
    }
    let bytes = std::fs::read(path).ok()?;
    let artifact: Artifact = serde_json::from_slice(&bytes).ok()?;
    Some(artifact.routes)
}

/// Rebuild the nested route tree from the flattened WIT list. Each entry's
/// `parent` is the index of its parent; every parent precedes its children, so a
/// single backward pass reattaches children. Matches the `introspect_routes` bin.
fn rebuild_tree(flat: &[wit::RouteDescriptor]) -> Vec<RouteNode> {
    let mut nodes: Vec<(Option<u32>, RouteNode)> = flat
        .iter()
        .map(|r| {
            (
                r.parent,
                RouteNode {
                    template: r.template.clone(),
                    kind: r.kind.into(),
                    description: r.description.clone(),
                    representations: r.representations.clone(),
                    children: Vec::new(),
                },
            )
        })
        .collect();

    let mut roots = Vec::new();
    while let Some((parent, node)) = nodes.pop() {
        match parent {
            Some(idx) => {
                let idx = idx as usize;
                if idx < nodes.len() {
                    nodes[idx].1.children.insert(0, node);
                } else {
                    roots.insert(0, node);
                }
            },
            None => roots.insert(0, node),
        }
    }
    roots
}

// ---------------------------------------------------------------------------
// Man-page rendering. CLI presentation only: no human labels leak into the
// schema types above.
// ---------------------------------------------------------------------------

fn render_man_page(meta: &ProviderMetadata) {
    let manifest = &meta.manifest;

    // NAME — display name accented with the provider's brand color.
    section("NAME");
    let title = match &manifest.color {
        Some(hex) => style::hex_accent(&manifest.display_name, hex),
        None => style::bold(&manifest.display_name),
    };
    let version = meta.info.as_ref().map(|i| i.version.as_str());
    let id_line = match version {
        Some(version) => format!("{}  {}", manifest.id, style::dim(format!("v{version}"))),
        None => manifest.id.clone(),
    };
    anstream::println!("  {title}");
    anstream::println!("  {}", style::dim(id_line));

    // DESCRIPTION — prefer the live provider-info description.
    if let Some(info) = &meta.info
        && !info.description.trim().is_empty()
    {
        section("DESCRIPTION");
        anstream::println!("  {}", info.description);
    }

    section("MOUNT");
    anstream::println!("  {}", style::dim("default mount path"));
    anstream::println!("  {}", manifest.default_mount);

    if let Some(contract) = &manifest.contract {
        section("CONTRACT");
        anstream::println!("  {:<6}{}", "WIT", contract.wit);
        anstream::println!("  {:<6}{}", "SDK", contract.sdk);
    }

    if !manifest.capabilities.is_empty() {
        section("CAPABILITIES");
        render_capabilities(&manifest.capabilities);
    }

    if let Some(auth) = &manifest.auth {
        section("AUTHENTICATION");
        render_auth(auth);
    }

    section("ROUTES");
    render_routes(&meta.routes);

    section("CONFIG");
    match &manifest.config_schema {
        Some(_) => anstream::println!(
            "  {}",
            "a config schema is declared (see `omnifs init` to configure)"
        ),
        None => anstream::println!("  {}", style::dim("no configuration required")),
    }
}

fn render_capabilities(capabilities: &[CapabilityEntry]) {
    for entry in capabilities {
        anstream::println!(
            "  {}: {}",
            style::accent(capability_label(entry)),
            capability_value(entry)
        );
        anstream::println!("    {}", style::dim(entry.why()));
    }
}

fn render_auth(auth: &ProviderAuthManifest) {
    for (key, scheme) in &auth.schemes {
        let is_default = key == &auth.default;
        let marker = if is_default {
            style::dim(" (default)")
        } else {
            String::new()
        };
        let (kind, summary_line) = match scheme {
            AuthScheme::None => ("none", String::new()),
            AuthScheme::StaticToken(token) => ("static token", token.description.clone()),
            AuthScheme::Oauth(oauth) => ("oauth", oauth_flow_label(&oauth.flow).to_string()),
        };
        anstream::println!("  {}{marker} {}", style::accent(key), style::dim(kind));
        if !summary_line.is_empty() {
            anstream::println!("    {summary_line}");
        }
        let guidance = auth.guidance_for(key);
        if let Some(summary) = guidance.summary {
            anstream::println!("    {}", style::dim(summary));
        }
    }
}

fn oauth_flow_label(flow: &OAuthFlow) -> &'static str {
    match flow {
        OAuthFlow::PkceLoopback(_) => "PKCE loopback flow",
        OAuthFlow::PkceManualCode(_) => "PKCE manual-code flow",
        OAuthFlow::ClientSideToken(_) => "client-side token flow",
        OAuthFlow::DeviceCode(_) => "device-code flow",
    }
}

fn render_routes(introspection: &RouteIntrospection) {
    match introspection {
        RouteIntrospection::RequiresConfig { reason } => {
            anstream::println!(
                "  {}",
                style::warn("routes: require provider configuration")
            );
            anstream::println!("  {}", style::dim(reason));
            return;
        },
        RouteIntrospection::FromArtifact { .. } => {
            anstream::println!(
                "  {}",
                style::dim("(from committed omnifs.routes.json; provider is config-gated)")
            );
        },
        RouteIntrospection::Introspected { .. } => {},
    }

    let routes = introspection.routes();
    if routes.is_empty() {
        anstream::println!("  {}", style::dim("no routes"));
        return;
    }
    for node in routes {
        render_route_node(node, 1);
    }
}

fn render_route_node(node: &RouteNode, depth: usize) {
    let indent = "  ".repeat(depth);
    let kind = style::dim(format!("[{}]", node.kind.label()));
    anstream::println!("{indent}{} {kind}", node.template);
    if let Some(description) = &node.description {
        anstream::println!("{indent}  {}", style::dim(description));
    }
    if !node.representations.is_empty() {
        anstream::println!(
            "{indent}  {} {}",
            style::dim("representations:"),
            node.representations.join(", ")
        );
    }
    for child in &node.children {
        render_route_node(child, depth + 1);
    }
}

fn section(title: &str) {
    anstream::println!();
    anstream::println!("{}", style::bold(title));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release wasm artifacts live here in a source checkout; the live-data
    /// tests below need them and skip cleanly when they are absent.
    fn release_wasm(file: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release")
            .join(file);
        path.is_file().then_some(path)
    }

    /// Regression: `provider info test` must surface the provider's branding
    /// color, a route table with descriptions, and object representations — the
    /// payoff of the colors/descriptions/routes branches. Asserts on the data
    /// model, not the rendered string, so it is not brittle to formatting.
    #[test]
    fn assembled_metadata_carries_color_routes_and_descriptions() {
        let Some(wasm) = release_wasm("test_provider.wasm") else {
            eprintln!("skipping: test_provider.wasm not built");
            return;
        };

        let meta = assemble_provider_metadata(&wasm).expect("assemble test provider metadata");

        // Branding color from the static manifest (colors branch).
        assert_eq!(meta.manifest.color.as_deref(), Some("#6B7280"));
        assert_eq!(meta.manifest.id, "test-provider");

        // Routes introspected live, not from an artifact (the test provider
        // starts under empty config).
        let routes = match &meta.routes {
            RouteIntrospection::Introspected { routes } => routes,
            other => panic!("expected live introspection, got {other:?}"),
        };

        // The `/items` dir route carries a description (descriptions branch).
        let items = routes
            .iter()
            .find(|r| r.template == "/items")
            .expect("/items route present");
        assert!(
            items.description.is_some(),
            "/items should carry a description"
        );

        // The object route is a top-level entry whose object leaves hang off it
        // as children; it carries both a description and representations.
        let item = routes
            .iter()
            .find(|r| r.template == "/items/{filter}/{number}")
            .expect("object route present");
        assert!(matches!(item.kind, RouteKind::Object));
        assert!(
            item.representations.iter().any(|r| r == "item.json"),
            "object route should expose representations"
        );
        assert!(
            item.description.is_some(),
            "object route should carry a description"
        );
        assert!(
            item.children
                .iter()
                .any(|c| c.template == "/items/{filter}/{number}/title"),
            "object leaves should nest under the object route"
        );
    }
}
