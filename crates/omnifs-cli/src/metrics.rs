//! Best-effort CLI-local dogfood metrics.

use std::io::Write as _;

use serde::Serialize;

use omnifs_bootstrap::Profile;

const METRICS_DIR: &str = "metrics";
const CLI_FILE: &str = "cli.jsonl";

#[derive(Serialize)]
struct CliRecord<'a> {
    ts: String,
    cmd: &'a str,
    exit: i32,
}

fn enabled(profile: &Profile) -> bool {
    omnifs_bootstrap::profile_config::read(profile.root()).is_ok_and(|config| {
        config.metrics.enabled
            && !matches!(
                std::env::var("OMNIFS_METRICS")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "0" | "false" | "no" | "off"
            )
    })
}

fn append(profile: &Profile, file: &str, value: &impl Serialize) {
    let dir = profile.root().join(METRICS_DIR);
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(file);
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            handle.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
        line.push('\n');
        handle.write_all(line.as_bytes())
    })();
    if let Err(error) = result {
        tracing::debug!(%error, file, "metrics write skipped");
    }
}

pub(crate) fn record_cli_exit(cmd: &str, exit: i32) {
    let Ok(profile) = Profile::resolve() else {
        return;
    };
    // Metrics need only the profile config and profile-local metrics
    // directory. A normal CLI invocation must not prepare filesystem state as
    // a side effect.
    if !enabled(&profile) {
        return;
    }
    let ts = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
    append(&profile, CLI_FILE, &CliRecord { ts, cmd, exit });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_write_only_the_profile_metrics_tree() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::under_root(dir.path());
        append(
            &profile,
            CLI_FILE,
            &CliRecord {
                ts: "2026-01-01T00:00:00Z".to_owned(),
                cmd: "status",
                exit: 0,
            },
        );
        assert!(dir.path().join("metrics/cli.jsonl").is_file());
        assert!(!dir.path().join("client").exists());
    }
}
