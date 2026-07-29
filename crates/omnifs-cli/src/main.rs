//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as profile inspection utilities.

// The output drift gate. Direct std printing is denied crate-wide so a new
// command cannot bypass the `ui` toolkit; the anstream print macros are denied
// through `.clippy.toml`'s `disallowed-macros`. Files under `src/ui/` own the
// sanctioned render paths; raw logs and completions are the only passthroughs.
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod auth;
mod capability;
mod cli;
mod client_dir;
mod client_fs_state;
mod client_state;
mod commands;
mod daemon_teardown;
mod docker;
mod error;
mod filesystem_driver;
mod guest_image_pull;
mod host_fs;
mod image;
mod inventory;
mod libkrun_runner;
mod metrics;
mod mutation;
mod process;
mod provider_catalog;
mod provider_resolver;
mod rpc;
mod status;
mod token_source;
mod ui;

use clap::Parser;
use cli::Cli;
use error::ExitCode;
use ui::output::Output;

fn main() {
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
    match cli.command {
        Some(cli::Commands::RunFs(args)) => {
            let code = match omnifs_thin::run(args) {
                Ok(()) => ExitCode::Success.code(),
                Err(error) => {
                    ui::eprint_raw(&format!("Error: {error:#}\n"));
                    ExitCode::GenericFailure.code()
                },
            };
            if code != 0 {
                std::process::exit(code);
            }
        },
        command => {
            let cli = Cli { command, ..cli };
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build the CLI tokio runtime")
                .block_on(cli_main(cli));
        },
    }
}

async fn cli_main(cli: Cli) {
    let inspector = cli
        .runs_daemon()
        .then(omnifs_engine::init_global_from_env)
        .flatten();
    if cli.runs_daemon() {
        if let Err(error) = omnifs_daemon::init_tracing(inspector.as_ref()) {
            ui::eprint_raw(&format!("Error: initialize daemon logging: {error:#}\n"));
            std::process::exit(ExitCode::GenericFailure.code());
        }
    } else {
        init_tracing(cli.verbose, inspector.as_ref());
    }
    let command_path = cli.command_path();
    let output = Output::new(cli.output, cli.quiet)
        .with_command(command_path)
        .with_no_input(cli.no_input)
        .with_yes(cli.yes);
    // Capture the usage label before `run` consumes `cli`. `None` for the
    // internal `daemon` subcommand, which records its own usage stream.
    let usage_label = cli.usage_label();
    match Box::pin(run(cli, output.clone())).await {
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
                } else if !output.is_closed() {
                    // A consent decline already printed its own closing line
                    // (`Kept everything as it was.`) before
                    // returning this same cancellation, so the generic
                    // `canceled` line would be a second, redundant close.
                    ui::eprint_raw(&format!(
                        "{}\n",
                        ui::style::dim("canceled", ui::style::Stream::Stderr)
                    ));
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
                ui::eprint_raw(&ui::render::render_error(&error));
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
    Box::pin(cli.run(output)).await
}
