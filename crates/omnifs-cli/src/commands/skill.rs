//! Skill installation commands for agent harnesses.

use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use crate::ui::output::Output;

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
    pub fn run(self, output: &Output) -> anyhow::Result<()> {
        output.require_human("skill")?;
        match self.command {
            SkillCommand::Install { target } => target.install(output),
        }
    }
}

impl InstallTarget {
    fn install(self, output: &Output) -> anyhow::Result<()> {
        match self {
            Self::ClaudeCode => {
                install_claude_code(std::env::var_os("HOME").map(PathBuf::from), output)
            },
        }
    }
}

fn install_claude_code(home: Option<PathBuf>, output: &Output) -> anyhow::Result<()> {
    let Some(home) = home else {
        anyhow::bail!(
            "Could not determine ~/.claude; source skill is at {}",
            source_path().display()
        );
    };
    install_claude_code_in(&home, output)
}

fn install_claude_code_in(home: &Path, output: &Output) -> anyhow::Result<()> {
    let target = home.join(".claude").join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&target)
        .with_context(|| format!("create skill directory {}", target.display()))?;
    let skill = target.join("SKILL.md");
    std::fs::write(&skill, USAGE_SKILL)
        .with_context(|| format!("write skill file {}", skill.display()))?;
    let key_width = crate::ui::render::key_field_width(&["skill"]);
    output.ledger_row(
        &crate::ui::render::LedgerRow::new(
            crate::ui::style::Glyph::Done,
            "skill",
            format!("installed at {}", target.display()),
        ),
        key_width,
    );
    output.outro(format!("`{SKILL_NAME}` is ready for your agent harness."));
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
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let error =
            install_claude_code(None, &output).expect_err("missing HOME must fail, not no-op");
        let message = error.to_string();
        assert!(message.contains("Could not determine ~/.claude"));
        assert!(message.contains("source skill is at"));
    }
}
