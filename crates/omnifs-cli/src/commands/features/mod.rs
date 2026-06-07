//! `omnifs features`: opt-in host-side integrations.
//!
//! Currently one feature, `yazi`, which installs the inspector trace
//! previewer plugin into the user's Yazi config and registers it. The plugin
//! sources are compiled in (see `integrations/yazi/`) so a published binary
//! can install them without a source checkout.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

const PLUGIN_MAIN_LUA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/yazi/omnifs-inspect.yazi/main.lua"
));
const PLUGIN_README: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/yazi/omnifs-inspect.yazi/README.md"
));

const PLUGIN_DIR_NAME: &str = "omnifs-inspect.yazi";
const PREVIEWER_NAME_GLOB: &str = "*.omnifs-inspect";
const PREVIEWER_RUN: &str = "omnifs-inspect";
const SENTINEL_FILE: &str = "inspector.omnifs-inspect";

#[derive(Args, Debug, Clone)]
pub struct FeaturesArgs {
    #[command(subcommand)]
    command: FeaturesCommand,
}

#[derive(Subcommand, Debug, Clone)]
enum FeaturesCommand {
    /// Install and configure an integration.
    Add {
        #[arg(value_enum)]
        feature: Feature,
    },
    /// Remove an integration.
    Remove {
        #[arg(value_enum)]
        feature: Feature,
    },
    /// Show the install state of each integration.
    List,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum Feature {
    /// Yazi inspector trace previewer.
    Yazi,
}

impl FeaturesArgs {
    pub fn run(self) -> Result<()> {
        match self.command {
            FeaturesCommand::Add {
                feature: Feature::Yazi,
            } => add_yazi(),
            FeaturesCommand::Remove {
                feature: Feature::Yazi,
            } => remove_yazi(),
            FeaturesCommand::List => list(),
        }
    }
}

/// Resolve Yazi's config directory: `YAZI_CONFIG_HOME`, then the platform
/// default (`%AppData%\yazi\config` on Windows, `$XDG_CONFIG_HOME/yazi` or
/// `$HOME/.config/yazi` elsewhere).
fn yazi_config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("YAZI_CONFIG_HOME") {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("AppData") {
        return Ok(PathBuf::from(appdata).join("yazi").join("config"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("yazi"));
    }
    let home =
        std::env::var_os("HOME").context("HOME is not set; cannot locate the Yazi config dir")?;
    Ok(PathBuf::from(home).join(".config").join("yazi"))
}

fn add_yazi() -> Result<()> {
    let config = yazi_config_dir()?;
    let plugin_dir = config.join("plugins").join(PLUGIN_DIR_NAME);
    fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("create plugin dir {}", plugin_dir.display()))?;
    fs::write(plugin_dir.join("main.lua"), PLUGIN_MAIN_LUA).context("write main.lua")?;
    fs::write(plugin_dir.join("README.md"), PLUGIN_README).context("write README.md")?;

    let yazi_toml = config.join("yazi.toml");
    register_previewer(&yazi_toml)?;

    let sentinel = config.join(SENTINEL_FILE);
    if !sentinel.exists() {
        fs::write(&sentinel, "omnifs inspector — hover me in Yazi\n")
            .with_context(|| format!("write sentinel {}", sentinel.display()))?;
    }

    println!("Installed the Yazi inspector previewer.");
    println!("  plugin:   {}", plugin_dir.display());
    println!("  config:   {}", yazi_toml.display());
    println!("  sentinel: {}", sentinel.display());
    println!();
    println!("Start the daemon (`omnifs up` or `omnifs dev`), then open the sentinel:");
    println!("  yazi {}", sentinel.display());
    if which("yazi").is_none() {
        println!();
        println!("note: `yazi` was not found on PATH; install it to use the previewer.");
    }
    Ok(())
}

fn remove_yazi() -> Result<()> {
    let config = yazi_config_dir()?;
    let plugin_dir = config.join("plugins").join(PLUGIN_DIR_NAME);
    if plugin_dir.exists() {
        fs::remove_dir_all(&plugin_dir)
            .with_context(|| format!("remove plugin dir {}", plugin_dir.display()))?;
    }
    let yazi_toml = config.join("yazi.toml");
    if yazi_toml.exists() {
        unregister_previewer(&yazi_toml)?;
    }
    let _ = fs::remove_file(config.join(SENTINEL_FILE));
    println!("Removed the Yazi inspector previewer.");
    Ok(())
}

fn list() -> Result<()> {
    let config = yazi_config_dir()?;
    let installed = config.join("plugins").join(PLUGIN_DIR_NAME).is_dir()
        && previewer_registered(&config.join("yazi.toml"))?;
    println!(
        "yazi   {}   inspector trace previewer",
        if installed { "installed" } else { "—" }
    );
    Ok(())
}

/// Idempotently add our `prepend_previewers` entry to `yazi.toml`, preserving
/// any existing config and formatting.
fn register_previewer(path: &Path) -> Result<()> {
    let mut doc = load_doc(path)?;

    let plugin = doc
        .entry("plugin")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("`plugin` is not a table in yazi.toml")?;
    let arr = plugin
        .entry("prepend_previewers")
        .or_insert_with(|| Item::Value(Value::Array(Array::new())))
        .as_array_mut()
        .context("`plugin.prepend_previewers` is not an array in yazi.toml")?;

    if !arr.iter().any(is_our_entry) {
        let mut entry = InlineTable::new();
        entry.insert("name", PREVIEWER_NAME_GLOB.into());
        entry.insert("run", PREVIEWER_RUN.into());
        arr.push(Value::InlineTable(entry));
    }

    write_doc(path, &doc)
}

fn unregister_previewer(path: &Path) -> Result<()> {
    let mut doc = load_doc(path)?;
    if let Some(arr) = doc
        .get_mut("plugin")
        .and_then(Item::as_table_mut)
        .and_then(|t| t.get_mut("prepend_previewers"))
        .and_then(Item::as_array_mut)
    {
        arr.retain(|v| !is_our_entry(v));
    }
    write_doc(path, &doc)
}

fn previewer_registered(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let doc = load_doc(path)?;
    Ok(doc
        .get("plugin")
        .and_then(Item::as_table)
        .and_then(|t| t.get("prepend_previewers"))
        .and_then(Item::as_array)
        .is_some_and(|arr| arr.iter().any(is_our_entry)))
}

fn is_our_entry(v: &Value) -> bool {
    v.as_inline_table()
        .is_some_and(|t| t.get("run").and_then(Value::as_str) == Some(PREVIEWER_RUN))
}

fn load_doc(path: &Path) -> Result<DocumentMut> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    existing
        .parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    fs::write(path, doc.to_string()).with_context(|| format!("write {}", path.display()))
}

/// Minimal PATH lookup for the post-install hint. Best-effort, Unix-shaped.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_is_idempotent_and_preserves_other_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = dir.path().join("yazi.toml");
        fs::write(
            &toml,
            "[mgr]\nshow_hidden = true\n\n[plugin]\nprepend_previewers = [\n  { name = \"*.md\", run = \"md\" },\n]\n",
        )
        .expect("seed");

        register_previewer(&toml).expect("register");
        register_previewer(&toml).expect("register again");

        let body = fs::read_to_string(&toml).expect("read");
        // User's unrelated config survives.
        assert!(body.contains("show_hidden = true"));
        assert!(body.contains("run = \"md\""));
        // Our entry is present exactly once (match the `run = ` token, since
        // the bare id also appears inside the `*.omnifs-inspect` name glob).
        assert_eq!(body.matches("run = \"omnifs-inspect\"").count(), 1);
        assert!(previewer_registered(&toml).expect("query"));

        unregister_previewer(&toml).expect("unregister");
        assert!(!previewer_registered(&toml).expect("query"));
        // Removal leaves the user's previewer intact.
        assert!(
            fs::read_to_string(&toml)
                .expect("read")
                .contains("run = \"md\"")
        );
    }

    #[test]
    fn register_creates_file_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = dir.path().join("nested").join("yazi.toml");
        register_previewer(&toml).expect("register");
        assert!(previewer_registered(&toml).expect("query"));
    }
}
