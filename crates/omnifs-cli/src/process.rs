//! Unix process probes shared by detached runtime owners.

use std::process::{Command, Stdio};

/// Whether `kill -0` reports `pid` as a live process.
pub(crate) fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::is_alive;

    #[test]
    fn distinguishes_current_and_exited_processes() {
        assert!(is_alive(std::process::id()));

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!is_alive(pid));
    }
}
