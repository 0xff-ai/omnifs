//! Ambient credential detection: a generic interpreter over a provider's
//! declared `authn::AmbientSource`s. The host never hardcodes a provider's
//! probe here; it only knows how to read an env var or run a declared
//! command and take its trimmed stdout as a token. The init flow can offer
//! to import a detected credential instead of starting a fresh auth flow.

use omnifs_workspace::authn::{AmbientKind, AmbientSource, AuthManifest};
use secrecy::SecretString;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long an ambient command may run before it is killed and ignored.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum DetectedCredential {
    /// An environment variable carries a token.
    EnvVar { name: String, value: SecretString },
    /// A declared command's trimmed stdout carried a token. `note` is the
    /// provider-supplied human-facing description of the source.
    Command { note: String, value: SecretString },
}

/// Reads every ambient source declared across `manifest`'s static-token
/// schemes and returns the credentials found, in declaration order.
pub fn detect(manifest: Option<&AuthManifest>) -> Vec<DetectedCredential> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    manifest.ambient_sources().filter_map(read_source).collect()
}

fn read_source(source: &AmbientSource) -> Option<DetectedCredential> {
    match &source.kind {
        AmbientKind::EnvVar { name } => {
            let value = std::env::var_os(name)?;
            if value.is_empty() {
                return None;
            }
            Some(DetectedCredential::EnvVar {
                name: name.clone(),
                value: SecretString::from(value.to_string_lossy().into_owned()),
            })
        },
        AmbientKind::Command { argv } => {
            let token = run_command(argv)?;
            Some(DetectedCredential::Command {
                note: source.note.clone(),
                value: SecretString::from(token),
            })
        },
    }
}

/// Runs `argv` as a plain argv exec (never a shell, no string interpolation)
/// and returns its trimmed stdout if it exits successfully within
/// [`COMMAND_TIMEOUT`]. A command that is still running at the deadline is
/// killed and treated as not found.
fn run_command(argv: &[String]) -> Option<String> {
    let (binary, rest) = argv.split_first()?;
    let mut child = Command::new(binary)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout on a separate thread so a chatty command can't deadlock on
    // pipe backpressure while we poll for exit below. Killing the child on
    // timeout closes its stdout, so joining afterwards never blocks long.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let exited = wait_within(&mut child, COMMAND_TIMEOUT);
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = reader.join().ok()?;
    if !exited {
        return None;
    }

    let token = String::from_utf8_lossy(&output).trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// Polls `child` until it exits or `timeout` elapses. Returns whether it
/// exited successfully within the deadline.
fn wait_within(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::authn::{AuthScheme, StaticTokenScheme};
    use secrecy::ExposeSecret;

    fn manifest_with(sources: Vec<AmbientSource>) -> AuthManifest {
        AuthManifest {
            schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
                key: "pat".to_string(),
                header_name: None,
                value_prefix: "Bearer ".to_string(),
                description: "test".to_string(),
                inject_domains: vec![],
                creation_url: None,
                validation: None,
                ambient_sources: sources,
            })],
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_env_var_source() {
        // SAFETY: this is the only test in this module and no other test in
        // the crate mutates DETECT_TEST_ENV_VAR, so no concurrent env write
        // can race here.
        unsafe {
            std::env::set_var("DETECT_TEST_ENV_VAR", "token-value");
        }
        let manifest = manifest_with(vec![AmbientSource::env_var("DETECT_TEST_ENV_VAR")]);
        let found = detect(Some(&manifest));
        assert!(
            found.iter().any(|d| matches!(
                d,
                DetectedCredential::EnvVar { name, .. } if name == "DETECT_TEST_ENV_VAR"
            )),
            "found: {found:?}"
        );
        // SAFETY: same rationale as the set_var above.
        unsafe {
            std::env::remove_var("DETECT_TEST_ENV_VAR");
        }
    }

    #[test]
    fn detect_command_source() {
        let manifest = manifest_with(vec![
            AmbientSource::command(["echo", "  command-token  "]).note("echo probe"),
        ]);
        let found = detect(Some(&manifest));
        let credential = found
            .into_iter()
            .find(|d| matches!(d, DetectedCredential::Command { .. }))
            .expect("command credential detected");
        let DetectedCredential::Command { note, value } = credential else {
            unreachable!()
        };
        assert_eq!(note, "echo probe");
        assert_eq!(value.expose_secret(), "command-token");
    }

    #[test]
    fn detect_command_source_times_out() {
        let manifest = manifest_with(vec![AmbientSource::command(["sleep", "10"])]);
        let start = Instant::now();
        let found = detect(Some(&manifest));
        assert!(found.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "expected the command to be killed at the timeout, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn detect_returns_empty_without_manifest() {
        assert!(detect(None).is_empty());
    }
}
