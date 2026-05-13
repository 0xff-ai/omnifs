//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as plugin introspection utilities.

mod mount_tree;

use anyhow::Context;
use clap::{Parser, Subcommand};
use omnifs_host::config::InstanceConfig;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "omnifs", about = "omnifs: a virtual filesystem for everything")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Mount {
        #[arg(long)]
        mount_point: String,
        #[arg(long)]
        config_dir: Option<String>,
        #[arg(long)]
        cache_dir: Option<String>,
    },
    Unmount {
        #[arg(long)]
        mount_point: String,
    },
    PluginInfo {
        /// Path to a .wasm provider component
        path: String,
    },
    /// Print the normalized mount graph for a provider.
    MountTree {
        /// Path to a .wasm provider component.
        path: String,
        #[arg(long)]
        tree: bool,
        #[arg(long)]
        paths: bool,
        #[arg(long)]
        by_type: bool,
    },
    /// Print mount and provider configuration status.
    Status {
        #[arg(long)]
        mount_point: Option<String>,
        #[arg(long)]
        config_dir: Option<String>,
        #[arg(long)]
        cache_dir: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPaths {
    mount_point: PathBuf,
    config_dir: PathBuf,
    providers_dir: PathBuf,
    plugin_dir: PathBuf,
    cache_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfo {
    source: String,
    mount_point: PathBuf,
    fs_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderReadyStatus {
    config_path: PathBuf,
    mount: String,
    plugin: String,
    plugin_present: bool,
    root_mount: bool,
    auth_count: usize,
    domain_count: usize,
    git_repo_count: usize,
    max_memory_mb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderConfigStatus {
    Ready(ProviderReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusReport {
    paths: ResolvedPaths,
    mount: Option<MountInfo>,
    providers: Vec<ProviderConfigStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RunningMountArgs {
    mount_point: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
}

fn default_mount_point() -> PathBuf {
    PathBuf::from("/omnifs")
}

fn default_config_dir() -> PathBuf {
    std::env::var_os("OMNIFS_CONFIG_DIR").map_or_else(
        || {
            dirs_next::home_dir()
                .unwrap_or_else(|| PathBuf::from("/root"))
                .join(".omnifs")
        },
        PathBuf::from,
    )
}

fn default_cache_dir(config_dir: &Path) -> PathBuf {
    std::env::var_os("OMNIFS_CACHE_DIR").map_or_else(|| config_dir.join("cache"), PathBuf::from)
}

fn parse_mount_command_args(args: &[String]) -> Option<RunningMountArgs> {
    let executable = Path::new(args.first()?).file_name()?.to_str()?;
    if executable != "omnifs" || args.get(1).map(String::as_str) != Some("mount") {
        return None;
    }

    let mut parsed = RunningMountArgs::default();
    let mut idx = 2;
    while idx < args.len() {
        let (key, inline_value) = args[idx].split_once('=').map_or_else(
            || (args[idx].as_str(), None),
            |(key, value)| (key, Some(value)),
        );

        if matches!(key, "--mount-point" | "--config-dir" | "--cache-dir") {
            let uses_inline_value = inline_value.is_some();
            let value = inline_value
                .map(PathBuf::from)
                .or_else(|| args.get(idx + 1).map(PathBuf::from));
            match key {
                "--mount-point" => parsed.mount_point = value,
                "--config-dir" => parsed.config_dir = value,
                "--cache-dir" => parsed.cache_dir = value,
                _ => {},
            }
            idx += if uses_inline_value { 1 } else { 2 };
            continue;
        }

        idx += 1;
    }

    Some(parsed)
}

fn infer_running_mount_args() -> Option<RunningMountArgs> {
    let proc_dir = Path::new("/proc");
    let entries = fs::read_dir(proc_dir).ok()?;

    for entry in entries.filter_map(Result::ok) {
        let file_name = entry.file_name();
        if file_name.to_string_lossy().parse::<u32>().is_err() {
            continue;
        }

        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }

        let args = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();

        if let Some(parsed) = parse_mount_command_args(&args) {
            return Some(parsed);
        }
    }

    None
}

fn resolve_paths(
    mount_point: Option<String>,
    config_dir: Option<String>,
    cache_dir: Option<String>,
) -> ResolvedPaths {
    let inferred = infer_running_mount_args().unwrap_or_default();
    let config_dir = config_dir
        .map(PathBuf::from)
        .or(inferred.config_dir)
        .unwrap_or_else(default_config_dir);
    let mount_point = mount_point
        .map(PathBuf::from)
        .or(inferred.mount_point)
        .unwrap_or_else(default_mount_point);
    let cache_dir = cache_dir
        .map(PathBuf::from)
        .or(inferred.cache_dir)
        .unwrap_or_else(|| default_cache_dir(&config_dir));
    let providers_dir = config_dir.join("providers");
    let plugin_dir = config_dir.join("plugins");

    ResolvedPaths {
        mount_point,
        config_dir,
        providers_dir,
        plugin_dir,
        cache_dir,
    }
}

fn decode_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let octal = &field[i + 1..i + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(char::from(value));
                i += 4;
                continue;
            }
        }

        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

fn parse_proc_mounts(contents: &str) -> Vec<MountInfo> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let mount_point = fields.next()?;
            let fs_type = fields.next()?;
            Some(MountInfo {
                source: decode_mount_field(source),
                mount_point: PathBuf::from(decode_mount_field(mount_point)),
                fs_type: decode_mount_field(fs_type),
            })
        })
        .collect()
}

fn find_mount(path: &Path) -> anyhow::Result<Option<MountInfo>> {
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(mounts) => mounts,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read /proc/mounts"),
    };
    let wanted = normalize_path(path);
    Ok(parse_proc_mounts(&mounts)
        .into_iter()
        .find(|mount| normalize_path(&mount.mount_point) == wanted))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn scan_provider_configs(
    providers_dir: &Path,
    plugin_dir: &Path,
) -> anyhow::Result<Vec<ProviderConfigStatus>> {
    if !providers_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = fs::read_dir(providers_dir)
        .with_context(|| format!("failed to read {}", providers_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to scan {}", providers_dir.display()))?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    files.sort();

    let mut providers = Vec::with_capacity(files.len());
    for config_path in files {
        match InstanceConfig::from_file(&config_path) {
            Ok(config) => {
                let plugin_path = plugin_dir.join(&config.plugin);
                providers.push(ProviderConfigStatus::Ready(ProviderReadyStatus {
                    config_path,
                    mount: config.mount,
                    plugin: config.plugin,
                    plugin_present: plugin_path.exists(),
                    root_mount: config.root_mount,
                    auth_count: config.auth.len(),
                    domain_count: config
                        .capabilities
                        .as_ref()
                        .and_then(|caps| caps.domains.as_ref())
                        .map_or(0, Vec::len),
                    git_repo_count: config
                        .capabilities
                        .as_ref()
                        .and_then(|caps| caps.git_repos.as_ref())
                        .map_or(0, Vec::len),
                    max_memory_mb: config.capabilities.and_then(|caps| caps.max_memory_mb),
                }));
            },
            Err(error) => providers.push(ProviderConfigStatus::Invalid {
                config_path,
                error: error.to_string(),
            }),
        }
    }

    Ok(providers)
}

fn collect_status(paths: ResolvedPaths) -> anyhow::Result<StatusReport> {
    Ok(StatusReport {
        mount: find_mount(&paths.mount_point)?,
        providers: scan_provider_configs(&paths.providers_dir, &paths.plugin_dir)?,
        paths,
    })
}

fn format_presence(path: &Path) -> &'static str {
    if path.exists() { "present" } else { "missing" }
}

fn render_status(report: &StatusReport) -> String {
    let mut out = String::new();
    let mounted = match &report.mount {
        Some(mount) => format!("yes (source={}, type={})", mount.source, mount.fs_type),
        None => String::from("no"),
    };

    let ready_count = report
        .providers
        .iter()
        .filter(|provider| {
            matches!(
                provider,
                ProviderConfigStatus::Ready(ProviderReadyStatus {
                    plugin_present: true,
                    ..
                })
            )
        })
        .count();

    let _ = writeln!(out, "mount point: {}", report.paths.mount_point.display());
    let _ = writeln!(out, "mounted: {mounted}");
    let _ = writeln!(
        out,
        "config dir: {} [{}]",
        report.paths.config_dir.display(),
        format_presence(&report.paths.config_dir)
    );
    let _ = writeln!(
        out,
        "providers dir: {} [{}]",
        report.paths.providers_dir.display(),
        format_presence(&report.paths.providers_dir)
    );
    let _ = writeln!(
        out,
        "plugin dir: {} [{}]",
        report.paths.plugin_dir.display(),
        format_presence(&report.paths.plugin_dir)
    );
    let _ = writeln!(
        out,
        "cache dir: {} [{}]",
        report.paths.cache_dir.display(),
        format_presence(&report.paths.cache_dir)
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "providers: {} configured, {} ready",
        report.providers.len(),
        ready_count
    );

    if report.providers.is_empty() {
        let _ = writeln!(out, "- none");
        return out;
    }

    for provider in &report.providers {
        match provider {
            ProviderConfigStatus::Ready(provider) => {
                let _ = write!(
                    out,
                    "- {}: plugin={} present={} auth={} domains={} git_repos={}",
                    provider.mount,
                    provider.plugin,
                    if provider.plugin_present { "yes" } else { "no" },
                    provider.auth_count,
                    provider.domain_count,
                    provider.git_repo_count
                );
                if provider.root_mount {
                    let _ = write!(out, " root=yes");
                }
                if let Some(max_memory_mb) = provider.max_memory_mb {
                    let _ = write!(out, " max_memory={max_memory_mb}MiB");
                }
                let _ = writeln!(out);
            },
            ProviderConfigStatus::Invalid { config_path, error } => {
                let name = config_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<unknown>");
                let _ = writeln!(out, "- {name}: invalid ({error})");
            },
        }
    }

    out
}

fn print_status(
    mount_point: Option<String>,
    config_dir: Option<String>,
    cache_dir: Option<String>,
) -> anyhow::Result<()> {
    let report = collect_status(resolve_paths(mount_point, config_dir, cache_dir))?;
    print!("{}", render_status(&report));
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        // NEW = span constructed (boundary marker for "started").
        // CLOSE = span dropped (boundary marker for "completed"), carrying
        // the framework's own elapsed time and any fields recorded over
        // the lifetime of the span.
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .init();

    let cli = Cli::parse();
    return run(cli);
}

fn run(cli: Cli) -> anyhow::Result<()> {
    use omnifs_host::mount;
    use omnifs_host::registry::ProviderRegistry;
    use omnifs_host::runtime::cloner::GitCloner;
    use std::sync::Arc;
    use tokio::runtime::Handle;

    match cli.command {
        Commands::Mount {
            mount_point,
            config_dir,
            cache_dir,
        } => {
            let config_path = config_dir.map_or_else(default_config_dir, PathBuf::from);
            let cache_path =
                cache_dir.map_or_else(|| default_cache_dir(&config_path), PathBuf::from);
            let plugin_dir = config_path.join("plugins");
            let mount_path = PathBuf::from(&mount_point);

            std::fs::create_dir_all(&mount_path)?;
            std::fs::create_dir_all(&cache_path)?;

            // Construct the shared GitCloner early and pass it to all components.
            let cloner = Arc::new(GitCloner::new(cache_path));

            info!(
                mount_point,
                config = %config_path.display(),
                cache = %cloner.cache_dir().display(),
                "loading providers"
            );

            let registry =
                ProviderRegistry::load(&config_path, &plugin_dir, &cloner, cloner.cache_dir())?;

            for mount_name in registry.mounts() {
                if let Some(runtime) = registry.get(&mount_name) {
                    match runtime.initialize() {
                        Ok(_) => info!(mount = mount_name, "provider initialized"),
                        Err(e) => warn!(mount = mount_name, error = %e, "init failed"),
                    }
                }
            }

            let registry = Arc::new(registry);
            let rt = Handle::current();
            registry.start_timers(&rt);

            info!(mount_point, "starting FUSE mount");
            mount::mount_blocking(&mount_path, &registry, rt)?;
            Ok(())
        },
        Commands::Unmount { mount_point } => {
            mount::unmount(&PathBuf::from(mount_point))?;
            Ok(())
        },
        Commands::PluginInfo { path: _ } => Err(anyhow::anyhow!("plugin info not yet implemented")),
        Commands::MountTree {
            path,
            tree,
            paths,
            by_type,
        } => {
            let views = crate::mount_tree::Views {
                tree,
                paths,
                by_type,
            }
            .with_defaults();
            let data = crate::mount_tree::read_from_wasm(std::path::Path::new(&path))?;
            print!("{}", crate::mount_tree::render(&data, &views));
            Ok(())
        },
        Commands::Status {
            mount_point,
            config_dir,
            cache_dir,
        } => print_status(mount_point, config_dir, cache_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderConfigStatus, ProviderReadyStatus, decode_mount_field, parse_mount_command_args,
        parse_proc_mounts, scan_provider_configs,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn decode_mount_field_unescapes_proc_mount_sequences() {
        assert_eq!(decode_mount_field(r"/tmp/with\040space"), "/tmp/with space");
        assert_eq!(decode_mount_field(r"weird\011tab"), "weird\ttab");
        assert_eq!(decode_mount_field(r"slash\134back"), r"slash\back");
    }

    #[test]
    fn parse_proc_mounts_reads_source_mount_and_fs_type() {
        let mounts = parse_proc_mounts("omnifs /omnifs fuse ro 0 0\n");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, "omnifs");
        assert_eq!(mounts[0].mount_point, std::path::PathBuf::from("/omnifs"));
        assert_eq!(mounts[0].fs_type, "fuse");
    }

    #[test]
    fn parse_mount_command_args_extracts_runtime_paths() {
        let args = vec![
            String::from("/usr/local/bin/omnifs"),
            String::from("mount"),
            String::from("--mount-point"),
            String::from("/omnifs"),
            String::from("--config-dir"),
            String::from("/root/.omnifs"),
            String::from("--cache-dir"),
            String::from("/tmp/omnifs-cache"),
        ];

        let parsed = parse_mount_command_args(&args).expect("mount args should parse");
        assert_eq!(parsed.mount_point, Some(PathBuf::from("/omnifs")));
        assert_eq!(parsed.config_dir, Some(PathBuf::from("/root/.omnifs")));
        assert_eq!(parsed.cache_dir, Some(PathBuf::from("/tmp/omnifs-cache")));
    }

    #[test]
    fn parse_mount_command_args_extracts_equals_form_paths() {
        let args = vec![
            String::from("/usr/local/bin/omnifs"),
            String::from("mount"),
            String::from("--mount-point=/omnifs"),
            String::from("--config-dir=/root/.omnifs"),
            String::from("--cache-dir=/tmp/omnifs-cache"),
        ];

        let parsed = parse_mount_command_args(&args).expect("mount args should parse");
        assert_eq!(parsed.mount_point, Some(PathBuf::from("/omnifs")));
        assert_eq!(parsed.config_dir, Some(PathBuf::from("/root/.omnifs")));
        assert_eq!(parsed.cache_dir, Some(PathBuf::from("/tmp/omnifs-cache")));
    }

    #[test]
    fn scan_provider_configs_reports_valid_and_invalid_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let providers_dir = temp.path().join("providers");
        let plugin_dir = temp.path().join("plugins");
        fs::create_dir_all(&providers_dir).expect("providers dir");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");

        fs::write(
            providers_dir.join("github.json"),
            r#"{
                "plugin": "omnifs_provider_github.wasm",
                "mount": "github",
                "auth": {
                    "type": "bearer-token",
                    "token_env": "GITHUB_TOKEN"
                },
                "capabilities": {
                    "domains": ["api.github.com"],
                    "git_repos": ["git@github.com:*"],
                    "max_memory_mb": 256
                }
            }"#,
        )
        .expect("write github config");
        fs::write(plugin_dir.join("omnifs_provider_github.wasm"), b"").expect("write plugin");
        fs::write(providers_dir.join("broken.json"), "{").expect("write broken config");

        let providers =
            scan_provider_configs(&providers_dir, &plugin_dir).expect("provider scan should work");

        assert_eq!(providers.len(), 2);
        match &providers[0] {
            ProviderConfigStatus::Invalid { config_path, .. } => {
                assert_eq!(
                    config_path.file_name().and_then(|name| name.to_str()),
                    Some("broken.json")
                );
            },
            other @ ProviderConfigStatus::Ready(_) => {
                panic!("expected invalid provider config, got {other:?}")
            },
        }

        match &providers[1] {
            ProviderConfigStatus::Ready(ProviderReadyStatus {
                mount,
                plugin,
                plugin_present,
                auth_count,
                domain_count,
                git_repo_count,
                max_memory_mb,
                ..
            }) => {
                assert_eq!(mount, "github");
                assert_eq!(plugin, "omnifs_provider_github.wasm");
                assert!(*plugin_present);
                assert_eq!(*auth_count, 1);
                assert_eq!(*domain_count, 1);
                assert_eq!(*git_repo_count, 1);
                assert_eq!(*max_memory_mb, Some(256));
            },
            other @ ProviderConfigStatus::Invalid { .. } => {
                panic!("expected ready provider config, got {other:?}")
            },
        }
    }
}
