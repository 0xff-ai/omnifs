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
mod control;
mod credential_target;
mod daemon_teardown;
mod error;
#[cfg(feature = "daemon")]
mod host_teardown;
mod inspector;
mod launch;
mod launch_backend;
mod launch_record;
mod mount_config;
mod mount_report;
mod mount_tree;
mod provider_bundle;
mod runtime;
mod stages;
mod status;
mod style;
mod telemetry;
#[cfg(test)]
mod test_support;
mod token_source;
mod ui;
mod upgrade;
mod workspace;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    ui::install_prompt_theme();
    // Capture the telemetry label before `run` consumes `cli`. `None` for the
    // internal `daemon` subcommand, which records `daemon.jsonl` itself.
    // Subcommands that `std::process::exit` on their own (shell, doctor) record
    // at their exit site; this covers every command that returns to `main`.
    let telemetry_label = cli.telemetry_label();
    match run(cli).await {
        Ok(exit_code) => {
            let code = exit_code.code();
            if let Some(cmd) = telemetry_label {
                telemetry::record_cli_exit(cmd, code);
            }
            if code != 0 {
                std::process::exit(code);
            }
        },
        Err(error) => {
            // A user cancel (picker Esc/Ctrl-C, or an inquire prompt mapped to
            // the same marker) is a normal exit, not a failure to spell out with
            // an `Error:` block: render one quiet line and leave.
            if ui::picker::is_canceled(&error) {
                let code = error::ExitCode::GenericFailure.code();
                if let Some(cmd) = telemetry_label {
                    telemetry::record_cli_exit(cmd, code);
                }
                anstream::eprintln!("{}", style::dim("canceled"));
                std::process::exit(code);
            }
            let exit_code = error::exit_code(&error).code();
            if let Some(cmd) = telemetry_label {
                telemetry::record_cli_exit(cmd, exit_code);
            }
            anstream::eprint!("{}", error::render(&error));
            std::process::exit(exit_code);
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

async fn run(cli: Cli) -> anyhow::Result<error::ExitCode> {
    cli.run().await
}
