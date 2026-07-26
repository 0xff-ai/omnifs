//! The credential-free out-of-process frontend runner.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "omnifs-thin",
    version,
    about = "Credential-free guest omnifs frontend runner"
)]
struct Cli {
    #[command(flatten)]
    runner: omnifs_thin::RunFrontendArgs,
}

fn main() -> anyhow::Result<()> {
    omnifs_thin::run(Cli::parse().runner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_help_lists_protocol_commands() {
        let error = Cli::try_parse_from(["omnifs-thin", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("nfs"));
        #[cfg(target_os = "linux")]
        assert!(help.contains("fuse"));
    }

    #[test]
    fn nfs_surface_lists_runtime_mount_and_state_arguments() {
        let error = Cli::try_parse_from(["omnifs-thin", "nfs", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("--runtime"));
        assert!(help.contains("--mount-point"));
        assert!(help.contains("--state-dir"));
        assert!(help.contains("--attach"));
        assert!(help.contains("--port"));
    }
}
