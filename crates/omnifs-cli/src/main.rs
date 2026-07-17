//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as workspace inspection utilities.

// The output drift gate. Direct std printing is denied crate-wide so a new
// command cannot bypass the `ui` toolkit; the anstream print macros are denied
// through `.clippy.toml`'s `disallowed-macros`. Files under `src/ui/` own the
// sanctioned render paths; raw logs and completions are the only passthroughs.
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod auth;
mod capability;
mod cli;
mod client;
mod commands;
mod credential_target;
mod daemon;
mod daemon_launch;
mod daemon_teardown;
mod docker;
mod error;
mod frontend_container;
mod guest_image_pull;
mod host_runner;
mod host_teardown;
mod image;
mod inspector;
mod inventory;
mod launch;
mod libkrun_runner;
mod metrics;
mod mount_config;
mod process;
mod provider_bundle;
mod provider_resolver;
mod provider_warmup;
mod stages;
mod status;
#[cfg(test)]
mod test_support;
mod token_source;
mod ui;

use clap::Parser;
use cli::{Cli, raw_command_path, raw_output_mode};
use error::ExitCode;
use ui::output::{ErrorEnvelope, ErrorPayload, ErrorVerdict, Output};

#[tokio::main]
async fn main() {
    let raw_args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let provisional_mode = raw_output_mode(raw_args.clone());
    // Map clap's parse outcome at the boundary, not per command: a usage error
    // exits 2, while `--help`/`--version` display exits 0. clap picks the right
    // stream (stdout for help/version, stderr for errors).
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let display_error = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            if let Some(mode) = provisional_mode
                && !display_error
            {
                let output = Output::new(mode, false);
                let envelope = ErrorEnvelope::new(
                    raw_command_path(raw_args),
                    ErrorVerdict::Failed,
                    ErrorPayload {
                        id: ExitCode::Usage.slug().to_owned(),
                        exit_code: ExitCode::Usage.code(),
                        message: error.to_string(),
                        causes: Vec::new(),
                        fix: None,
                        hints: Vec::new(),
                    },
                );
                if output.emit_error(envelope).is_err() {
                    let _ = error.print();
                }
                std::process::exit(ExitCode::Usage.code());
            }
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
    let inspector = cli
        .runs_daemon()
        .then(omnifs_engine::init_global_from_env)
        .flatten();
    init_tracing(cli.verbose, inspector.as_ref());
    ui::output::install_theme();
    let command_path = cli.command_path();
    let output = Output::new(cli.output, cli.quiet)
        .with_command(command_path)
        .with_no_input(cli.no_input)
        .with_yes(cli.yes);
    // Capture the usage label before `run` consumes `cli`. `None` for the
    // internal `daemon` subcommand, which records `daemon.jsonl` itself.
    // Subcommands that `std::process::exit` on their own (shell, doctor) record
    // at their exit site; this covers every command that returns to `main`.
    let usage_label = cli.usage_label();
    match run(cli, output.clone()).await {
        Ok(exit_code) => {
            let code = exit_code.code();
            if let Some(cmd) = usage_label {
                metrics::record_cli_exit(cmd, code);
            }
            if code != 0 {
                std::process::exit(code);
            }
        },
        Err(error) => {
            // A user cancel (Esc/Ctrl-C from any prompt) is a normal exit, not
            // a failure to spell out with an `Error:` block. It exits 130
            // (128 + SIGINT), the shell convention.
            if ui::prompt::is_canceled(&error) {
                let code = ExitCode::Canceled;
                if let Some(cmd) = usage_label {
                    metrics::record_cli_exit(cmd, code.code());
                }
                if output.is_structured() {
                    let _ = output.emit_error(error::canceled_envelope(command_path, "canceled"));
                } else {
                    ui::eprint_raw(&format!("{}\n", ui::style::dim("canceled")));
                }
                std::process::exit(code.code());
            }
            let exit_code = error::exit_code(&error).code();
            if let Some(cmd) = usage_label {
                metrics::record_cli_exit(cmd, exit_code);
            }
            // A structured invocation that fails before its result emits one
            // terminal error envelope on stdout rather than a human block.
            if output.is_structured() {
                if output
                    .emit_error(error::envelope(&error, command_path))
                    .is_err()
                {
                    ui::eprint_raw("Error: failed to serialize error document\n");
                }
            } else {
                ui::eprint_raw(&error::render(&error));
            }
            std::process::exit(exit_code);
        },
    }
}

fn init_tracing(verbose: u8, inspector: Option<&std::sync::Arc<omnifs_engine::Inspector>>) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
    use tracing_subscriber::util::SubscriberInitExt as _;

    use process::ProcessRole;
    // `-v` raises the foreground filter to the same baseline the spawned
    // daemon logs at; `-vv` turns on debug.
    let verbosity = match verbose {
        0 => ProcessRole::Cli.default_log_level(),
        1 => ProcessRole::Daemon.default_log_level(),
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(verbosity))
        .add_directive("omnifs_inspector=off".parse().expect("static directive"));
    let span_events = if verbose >= 2 {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_span_events(span_events)
        .with_filter(filter);
    let inspector_layer = inspector.map(|inspector| {
        inspector.layer().with_filter(filter_fn(|metadata| {
            metadata.target() == "omnifs_inspector"
        }))
    });
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(inspector_layer)
        .init();
}

async fn run(cli: Cli, output: Output) -> anyhow::Result<error::ExitCode> {
    cli.run(output).await
}
