//! The credential-free out-of-process filesystem runner.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "omnifs-thin",
    version,
    about = "Credential-free guest omnifs filesystem runner"
)]
struct Cli {
    #[command(flatten)]
    runner: omnifs_thin::RunFsArgs,
}

fn main() -> anyhow::Result<()> {
    omnifs_thin::run(Cli::parse().runner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_help_lists_named_flags() {
        let error = Cli::try_parse_from(["omnifs-thin", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("--name"));
        assert!(help.contains("--protocol"));
        assert!(help.contains("--runtime"));
    }

    #[test]
    fn flat_surface_lists_protocol_runtime_location_and_state_arguments() {
        let error = Cli::try_parse_from(["omnifs-thin", "--help"]).unwrap_err();
        let help = error.to_string();
        assert!(help.contains("--name"));
        assert!(help.contains("--protocol"));
        assert!(help.contains("--runtime"));
        assert!(help.contains("--location"));
        assert!(help.contains("--state-dir"));
        assert!(help.contains("--attach"));
        assert!(help.contains("--port"));
        assert!(!help.contains("Commands:"));
    }
}
