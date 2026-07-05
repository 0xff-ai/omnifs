//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as provider introspection utilities.

mod auth;
mod capability;
mod catalog;
mod cli;
mod client;
mod commands;
mod config;
mod credential_target;
mod daemon_teardown;
mod error;
#[cfg(feature = "daemon")]
mod host_teardown;
mod inspector;
mod launch;
mod launch_backend;
mod launch_record;
mod mount_report;
mod mount_tree;
mod provider_bundle;
mod runtime;
mod session;
mod status;
mod style;
mod telemetry;
mod test_support;
mod token_source;
mod upgrade;
mod workspace;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    // Capture the telemetry label before `run` consumes `cli`. `None` for the
    // internal `daemon` subcommand, which records `daemon.jsonl` itself.
    // Subcommands that `std::process::exit` on their own (shell, doctor) record
    // at their exit site; this covers every command that returns to `main`.
    let telemetry_label = cli.command.telemetry_label();
    match run(cli).await {
        Ok(()) => {
            if let Some(cmd) = telemetry_label {
                telemetry::record_cli_exit(cmd, 0);
            }
        },
        Err(error) => {
            if let Some(cmd) = telemetry_label {
                telemetry::record_cli_exit(cmd, 1);
            }
            anstream::eprint!("{}", error::render(&error));
            std::process::exit(1);
        },
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

    use launch_backend::{RunMode, default_daemon_log_level};
    // `-v` raises the foreground filter to the same baseline the spawned
    // daemon logs at; `-vv` turns on debug.
    let verbosity = match verbose {
        0 => default_daemon_log_level(RunMode::Foreground),
        1 => default_daemon_log_level(RunMode::Spawned),
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(verbosity));
    let mut builder = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter);
    if verbose >= 2 {
        builder = builder.with_span_events(FmtSpan::NEW | FmtSpan::CLOSE);
    }
    builder.init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    cli.command.run().await
}
