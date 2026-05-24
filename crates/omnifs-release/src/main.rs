use std::io::{self, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use omnifs_release::{
    RepoRoot, ShipPlan, bump_patch, check_changelog_pr, check_release_pr, create_github_release,
    prepare_release, read_workspace_version, release_notes_prompt, ship_plan,
};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "omnifs-release",
    about = "Prepare and validate omnifs releases"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Validate changelog and version state.
    Check {
        #[command(subcommand)]
        target: CheckTarget,
    },
    /// Print an LLM prompt with the commit range since the last tag (no embedded git log).
    Prompt,
    /// Create a release branch, bump versions, finalize changelog, and open a PR.
    Prepare {
        /// Target `SemVer` version. Defaults to a patch bump over the workspace version.
        #[arg(long)]
        version: Option<String>,
        /// Commit locally but do not push or open a PR.
        #[arg(long)]
        no_push: bool,
    },
    /// Decide whether main should ship and emit metadata for CI.
    ShipPlan {
        #[arg(long, default_value = "text")]
        format: OutputFormat,
    },
    /// Create the GitHub Release for the current workspace version.
    CreateGithubRelease {
        #[arg(long, default_value = "text")]
        format: OutputFormat,
    },
}

#[derive(Debug, Subcommand)]
enum CheckTarget {
    /// Feature PRs must update CHANGELOG [Unreleased] unless the PR has no-changelog.
    ChangelogPr {
        #[arg(long, default_value = "origin/main")]
        base: String,
        #[arg(long, default_value = "HEAD")]
        head: String,
    },
    /// Release PRs must contain a finalized version section and empty [Unreleased].
    ReleasePr,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo = RepoRoot::find(".")?;

    match cli.command {
        CliCommand::Check { target } => match target {
            CheckTarget::ChangelogPr { base, head } => check_changelog_pr(&repo, &base, &head)?,
            CheckTarget::ReleasePr => check_release_pr(&repo)?,
        },
        CliCommand::Prompt => {
            print!("{}", release_notes_prompt(&repo)?);
        },
        CliCommand::Prepare { version, no_push } => {
            let current = read_workspace_version(repo.root())?;
            let target = if let Some(version) = version {
                version
            } else {
                let suggested = bump_patch(&current)?;
                eprint!(
                    "Current workspace version: {current}\nSuggested patch release: {suggested}\nTarget version [{suggested}]: "
                );
                io::stderr().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let input = input.trim();
                if input.is_empty() {
                    suggested
                } else {
                    input.to_string()
                }
            };
            prepare_release(&repo, &target, !no_push)?;
        },
        CliCommand::ShipPlan { format } => {
            let plan = ship_plan(&repo)?;
            emit_plan(&plan, format)?;
        },
        CliCommand::CreateGithubRelease { format } => {
            let plan = ship_plan(&repo)?;
            let commit = create_github_release(&repo, &plan)?;
            match format {
                OutputFormat::Text => {
                    println!("created GitHub release {}", plan.tag);
                    println!("release_commit_sha={commit}");
                },
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "tag": plan.tag,
                            "version": plan.version,
                            "release_commit_sha": commit,
                        }))?
                    );
                },
            }
        },
    }

    Ok(())
}

fn emit_plan(plan: &ShipPlan, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Text => {
            if plan.should_ship {
                println!("should_ship=true");
                println!("version={}", plan.version);
                println!("tag={}", plan.tag);
            } else {
                println!("should_ship=false");
                println!("version={}", plan.version);
            }
        },
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(plan)?),
    }
    Ok(())
}
