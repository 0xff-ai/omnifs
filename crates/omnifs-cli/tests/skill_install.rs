//! Integration coverage for agent skill installation.

mod common;

use std::process::Command;

use common::omnifs_bin;

#[test]
fn skill_install_claude_code_round_trips_in_temp_home() {
    let home = tempfile::tempdir().unwrap();
    let omnifs_home = home.path().join(".omnifs");

    let output = Command::new(omnifs_bin())
        .args(["-q", "skill", "install", "claude-code"])
        .env("HOME", home.path())
        .env("OMNIFS_HOME", &omnifs_home)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn omnifs skill install claude-code");

    assert!(
        output.status.success(),
        "install failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = home
        .path()
        .join(".claude")
        .join("skills")
        .join("omnifs-usage")
        .join("SKILL.md");
    assert_eq!(
        std::fs::read_to_string(installed).unwrap(),
        include_str!("../../../skills/omnifs-usage/SKILL.md")
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Installed `omnifs-usage` skill"),
        "quiet must preserve the completed-operation receipt"
    );
}
