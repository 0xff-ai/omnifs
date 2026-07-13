//! The credential-free out-of-process frontend runner.
//!
//! `omnifs-thin` attaches to a host daemon's shared namespace through the
//! Omnifs VFS wire protocol and delegates protocol mechanics to the FUSE or
//! NFS frontend crates. It has no provider runtime, Wasmtime, daemon, or CLI
//! control plane.

#[cfg(target_os = "linux")]
mod fuse;
mod nfs;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "omnifs-thin",
    version,
    about = "Credential-free out-of-process omnifs frontend runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Attach to the daemon and serve a FUSE mount.
    #[cfg(target_os = "linux")]
    Fuse(fuse::Args),
    /// Attach to the daemon and serve an `NFSv4` loopback mount.
    Nfs(nfs::Args),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::Fuse(args) => fuse::run(args),
        Command::Nfs(args) => nfs::run(args),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn version_is_available_at_top_level() {
        let error = Cli::try_parse_from(["omnifs-thin", "--version"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert!(error.to_string().contains("omnifs-thin"));
    }

    #[test]
    fn top_level_help_lists_protocol_commands() {
        let error = Cli::try_parse_from(["omnifs-thin", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("nfs"));
        #[cfg(target_os = "linux")]
        assert!(help.contains("fuse"));
    }

    #[test]
    fn nfs_surface_requires_mount_and_state_directories() {
        let error = Cli::try_parse_from(["omnifs-thin", "nfs", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("--mount-point"));
        assert!(help.contains("--state-dir"));
        assert!(help.contains("--attach"));
        assert!(help.contains("--port"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fuse_surface_requires_mount_point_and_keeps_optional_attach_state() {
        let error = Cli::try_parse_from(["omnifs-thin", "fuse", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("--mount-point"));
        assert!(help.contains("--state-dir"));
        assert!(help.contains("--attach"));
    }
}
