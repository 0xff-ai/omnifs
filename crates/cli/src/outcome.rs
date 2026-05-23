//! Command exit semantics for the CLI process boundary.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandOutcome {
    Success,
    Exit(i32),
}

impl CommandOutcome {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Exit(code) => code,
        }
    }
}
