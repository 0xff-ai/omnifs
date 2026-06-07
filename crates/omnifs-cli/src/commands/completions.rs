//! `omnifs completions <shell>` — emit a clap-generated shell completion
//! script to stdout. `clap_complete` supports bash, elvish, fish, powershell,
//! and zsh; bash, zsh, and fish are officially documented.

use clap::Args;
use clap::CommandFactory;

use crate::cli::Cli;

#[derive(Args, Debug, Clone)]
pub struct CompletionsArgs {
    pub shell: clap_complete::Shell,
}

impl CompletionsArgs {
    pub fn run(self) {
        let mut cmd = Cli::command();
        clap_complete::generate(self.shell, &mut cmd, "omnifs", &mut std::io::stdout());
    }
}
