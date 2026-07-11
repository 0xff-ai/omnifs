//! Skill installation commands for agent harnesses.

use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

const USAGE_SKILL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../skills/omnifs-usage/SKILL.md"
));
const SKILL_NAME: &str = "omnifs-usage";

#[derive(Args, Debug, Clone)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SkillCommand {
    /// Install an omnifs usage skill for an agent harness.
    Install {
        #[arg(value_enum)]
        target: InstallTarget,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTarget {
    ClaudeCode,
}

impl SkillArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            SkillCommand::Install { target } => target.install(),
        }
    }
}

impl InstallTarget {
    fn install(self) -> anyhow::Result<()> {
        match self {
            Self::ClaudeCode => install_claude_code(std::env::var_os("HOME").map(PathBuf::from)),
        }
    }
}

fn install_claude_code(home: Option<PathBuf>) -> anyhow::Result<()> {
    let Some(home) = home else {
        anyhow::bail!(
            "Could not determine ~/.claude; source skill is at {}",
            source_path().display()
        );
    };
    install_claude_code_in(&home)
}

fn install_claude_code_in(home: &Path) -> anyhow::Result<()> {
    let target = home.join(".claude").join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&target)
        .with_context(|| format!("create skill directory {}", target.display()))?;
    let skill = target.join("SKILL.md");
    std::fs::write(&skill, USAGE_SKILL)
        .with_context(|| format!("write skill file {}", skill.display()))?;
    anstream::eprintln!("Installed `{SKILL_NAME}` skill at {}", target.display());
    Ok(())
}

fn source_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills")
        .join(SKILL_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_claude_code_errors_when_home_is_unset() {
        let error = install_claude_code(None).expect_err("missing HOME must fail, not no-op");
        let message = error.to_string();
        assert!(message.contains("Could not determine ~/.claude"));
        assert!(message.contains("source skill is at"));
    }
}
