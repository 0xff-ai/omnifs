//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as provider introspection utilities.

// The output drift gate. Direct std printing is denied crate-wide so a new
// command cannot bypass the `ui` toolkit; the anstream print macros are denied
// through `.clippy.toml`'s `disallowed-macros`. Files under `src/ui/` own the
// sanctioned render paths; raw logs and completions are the only passthroughs.
#![deny(clippy::print_stdout, clippy::print_stderr)]

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
mod frontend_backend;
mod frontend_container;
mod guest_image_pull;
#[cfg(feature = "daemon")]
mod host_teardown;
mod inspector;
mod krunkit_backend;
mod launch;
mod launch_backend;
#[cfg(feature = "daemon")]
mod local_backend;
mod mount_config;
mod mount_report;
mod mount_tree;
mod process;
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
use cli::{Cli, ProgressFormat};
use error::ExitCode;

#[tokio::main]
async fn main() {
    // Map clap's parse outcome at the boundary, not per command: a usage error
    // exits 2, while `--help`/`--version` display exits 0. clap picks the right
    // stream (stdout for help/version, stderr for errors).
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            let code = match error.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    ExitCode::Success
                },
                _ => ExitCode::Usage,
            };
            std::process::exit(code.code());
        },
    };
    init_tracing(cli.verbose);
    ui::session::install_theme();
    ui::output::configure(cli.progress == ProgressFormat::Json, cli.quiet);
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
            // A user cancel (picker Esc/Ctrl-C, or a prompt mapped to the same
            // marker) is a normal exit, not a failure to spell out with an
            // `Error:` block. It exits 130 (128 + SIGINT), the shell convention.
            if ui::picker::is_canceled(&error) {
                let code = ExitCode::Canceled;
                if let Some(cmd) = telemetry_label {
                    telemetry::record_cli_exit(cmd, code.code());
                }
                if ui::output::json_expected() {
                    emit_json_error(&error::to_json_for(code, "canceled"));
                } else {
                    ui::eprint_raw(&format!("{}\n", style::dim("canceled")));
                }
                std::process::exit(code.code());
            }
            let exit_code = error::exit_code(&error).code();
            if let Some(cmd) = telemetry_label {
                telemetry::record_cli_exit(cmd, exit_code);
            }
            // A `--json` command that failed before its document emits exactly
            // one JSON error document on stdout (with the stable `id`), rather
            // than the human `Error:` block on stderr.
            if ui::output::json_expected() {
                emit_json_error(&error::to_json(&error));
            } else {
                ui::eprint_raw(&error::render(&error));
            }
            std::process::exit(exit_code);
        },
    }
}

fn emit_json_error(document: &error::ErrorJson) {
    match ui::print_json(document) {
        Ok(()) => {},
        // Falling back to the human block keeps the failure visible if the
        // error document itself cannot be serialized.
        Err(_) => ui::eprint_raw("Error: failed to serialize error document\n"),
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

    use launch_backend::RunMode;
    // `-v` raises the foreground filter to the same baseline the spawned
    // daemon logs at; `-vv` turns on debug.
    let verbosity = match verbose {
        0 => RunMode::Foreground.default_log_level(),
        1 => RunMode::Spawned.default_log_level(),
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
