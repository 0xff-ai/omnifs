use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Macos,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmountCommand {
    program: &'static str,
    args: Vec<OsString>,
    failure_context: &'static str,
    fallback: Option<Box<UnmountCommand>>,
}

impl UnmountCommand {
    pub fn graceful(platform: Platform, mount_point: &Path) -> Self {
        match platform {
            Platform::Linux => Self::linux_graceful(mount_point),
            Platform::Macos => Self::macos_graceful(mount_point),
            Platform::Other => Self::other_graceful(mount_point),
        }
    }

    pub fn forced(platform: Platform, mount_point: &Path) -> Self {
        match platform {
            Platform::Linux => Self::linux_forced(mount_point),
            Platform::Macos => Self::macos_forced(mount_point),
            Platform::Other => Self::other_forced(mount_point),
        }
    }

    pub fn run(&self) -> Result<(), UnmountError> {
        self.run_with_output(CommandOutput::Inherit)
    }

    pub fn run_quiet(&self) -> Result<(), UnmountError> {
        self.run_with_output(CommandOutput::Quiet)
    }

    fn macos_graceful(mount_point: &Path) -> Self {
        Self {
            program: "diskutil",
            args: vec![
                OsString::from("unmount"),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "diskutil unmount",
            fallback: None,
        }
    }

    fn macos_forced(mount_point: &Path) -> Self {
        Self {
            program: "diskutil",
            args: vec![
                OsString::from("unmount"),
                OsString::from("force"),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "diskutil unmount force",
            fallback: None,
        }
    }

    fn linux_graceful(mount_point: &Path) -> Self {
        Self {
            program: "fusermount",
            args: vec![OsString::from("-u"), mount_point.as_os_str().to_owned()],
            failure_context: "fusermount -u",
            fallback: None,
        }
    }

    fn linux_forced(mount_point: &Path) -> Self {
        let mount_point = mount_point.as_os_str().to_owned();
        Self {
            program: "fusermount",
            args: vec![OsString::from("-uz"), mount_point.clone()],
            failure_context: "fusermount -uz",
            fallback: Some(Box::new(Self {
                program: "umount",
                args: vec![OsString::from("-f"), mount_point],
                failure_context: "umount -f",
                fallback: None,
            })),
        }
    }

    fn other_graceful(mount_point: &Path) -> Self {
        Self {
            program: "umount",
            args: vec![mount_point.as_os_str().to_owned()],
            failure_context: "umount",
            fallback: None,
        }
    }

    fn other_forced(mount_point: &Path) -> Self {
        Self {
            program: "umount",
            args: vec![OsString::from("-f"), mount_point.as_os_str().to_owned()],
            failure_context: "umount -f",
            fallback: None,
        }
    }

    fn run_with_output(&self, output: CommandOutput) -> Result<(), UnmountError> {
        match self.run_once(output) {
            Ok(()) => Ok(()),
            Err(error) => match self.fallback.as_deref() {
                Some(fallback) => fallback.run_with_output(output),
                None => Err(error),
            },
        }
    }

    fn run_once(&self, output: CommandOutput) -> Result<(), UnmountError> {
        let mut command = Command::new(self.program);
        command.args(&self.args);
        if output == CommandOutput::Quiet {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let status = command.status().map_err(|source| UnmountError::Run {
            context: self.failure_context,
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(UnmountError::Status {
                context: self.failure_context,
                status,
            })
        }
    }
}

#[derive(Debug, Error)]
pub enum UnmountError {
    #[error("{context}: {source}")]
    Run {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{context} exited with {status}")]
    Status {
        context: &'static str,
        status: ExitStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandOutput {
    Inherit,
    Quiet,
}

#[cfg(test)]
mod tests {
    use super::{Platform, UnmountCommand};
    use std::path::Path;

    fn args_as_strings(command: &UnmountCommand) -> Vec<String> {
        command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn linux_unmount_commands_use_fusermount_with_forced_fallback() {
        let graceful = UnmountCommand::graceful(Platform::Linux, Path::new("/mnt/omnifs"));
        assert_eq!(graceful.program, "fusermount");
        assert_eq!(args_as_strings(&graceful), vec!["-u", "/mnt/omnifs"]);
        assert!(graceful.fallback.is_none());

        let forced = UnmountCommand::forced(Platform::Linux, Path::new("/mnt/omnifs"));
        assert_eq!(forced.program, "fusermount");
        assert_eq!(args_as_strings(&forced), vec!["-uz", "/mnt/omnifs"]);
        let fallback = forced.fallback.as_deref().expect("forced fallback");
        assert_eq!(fallback.program, "umount");
        assert_eq!(args_as_strings(fallback), vec!["-f", "/mnt/omnifs"]);
    }

    #[test]
    fn macos_unmount_commands_use_diskutil() {
        let graceful = UnmountCommand::graceful(Platform::Macos, Path::new("/Volumes/omnifs"));
        assert_eq!(graceful.program, "diskutil");
        assert_eq!(
            args_as_strings(&graceful),
            vec!["unmount", "/Volumes/omnifs"]
        );

        let forced = UnmountCommand::forced(Platform::Macos, Path::new("/Volumes/omnifs"));
        assert_eq!(forced.program, "diskutil");
        assert_eq!(
            args_as_strings(&forced),
            vec!["unmount", "force", "/Volumes/omnifs"]
        );
    }
}
