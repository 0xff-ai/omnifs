//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as provider introspection utilities.

mod auth;
mod backend;
mod capability;
mod catalog;
mod cli;
mod client;
mod commands;
pub mod config;
mod credential_target;
mod daemon_teardown;
mod dev_mounts;
mod dev_support;
mod error;
mod host_teardown;
mod inspector;
mod kubernetes_testenv;
mod launch;
mod launch_backend;
mod launch_record;
mod live;
mod mount_report;
mod mount_tree;
mod provider_bundle;
mod runtime;
mod session;
mod status;
mod style;
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
    match run(cli).await {
        Ok(()) => {},
        Err(error) => {
            anstream::eprint!("{}", error::render(&error));
            std::process::exit(1);
        },
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

    let verbosity = match verbose {
        0 => "warn",
        1 => "info",
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
