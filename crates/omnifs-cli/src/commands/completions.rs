#![allow(clippy::disallowed_macros)] // permanent: raw shell-completion passthrough to stdout
//! `omnifs completions <shell>` — emit a clap-generated shell completion
//! script to stdout. `clap_complete` supports bash, elvish, fish, powershell,
//! and zsh; bash, zsh, and fish are officially documented.

use clap::Args;
use clap::CommandFactory;

use crate::cli::Cli;
use crate::ui::output::Output;

#[derive(Args, Debug, Clone)]
pub struct CompletionsArgs {
    pub shell: clap_complete::Shell,
}

impl CompletionsArgs {
    pub fn run(self, output: Output) -> anyhow::Result<()> {
        if output.is_structured() {
            anyhow::bail!("completions is a passthrough command and only supports human output")
        }
        let mut cmd = Cli::command();
        clap_complete::generate(self.shell, &mut cmd, "omnifs", &mut std::io::stdout());
        Ok(())
    }
}
