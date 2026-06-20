//! HTTP client for the daemon control API.
//!
//! The daemon listens on the container's published loopback port (or its
//! own loopback when running natively). `OMNIFS_DAEMON_ADDR` overrides the
//! `host:port` for non-default setups.

use anyhow::{Context as _, Result};
use omnifs_api::{API_MAJOR, API_MINOR, DaemonStatus, ReconcileReport, StopReport};
use std::time::Duration;

use crate::inspector::daemon_addr;

pub(crate) struct DaemonClient {
    base: String,
    http: reqwest::Client,
}

#[derive(Debug)]
pub(crate) enum DaemonProbe {
    Unreachable,
    Compatible(DaemonStatus),
}

impl DaemonClient {
    pub(crate) fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client with static config");
        Self {
            base: format!("http://{}", daemon_addr()),
            http,
        }
    }

    /// Probe for a daemon and verify its control API version in one step.
    ///
    /// Returns `Unreachable` when no daemon answers. Returns `Compatible` when
    /// the daemon's major matches ours (any minor). Refuses on major mismatch;
    /// emits a one-line warning when the minor differs and the major matches.
    pub(crate) async fn probe(&self) -> Result<DaemonProbe> {
        let Some(status) = self.status_optional().await? else {
            return Ok(DaemonProbe::Unreachable);
        };
        if status.api_major != API_MAJOR {
            let detail = if status.api_major == 0 {
                "this daemon predates major/minor API versioning".to_string()
            } else {
                format!(
                    "daemon speaks control API v{}.{}",
                    status.api_major, status.api_minor
                )
            };
            anyhow::bail!(
                "{detail}; this CLI speaks v{API_MAJOR}.{API_MINOR} (daemon binary v{}). \
                 Stop it with `omnifs down`, or upgrade the runtime image so the CLI and \
                 daemon versions match, then rerun.",
                status.version,
            );
        }
        if status.api_minor != API_MINOR {
            anstream::eprintln!(
                "note: daemon API minor v{}.{}, CLI expects v{API_MAJOR}.{API_MINOR}; \
                 proceeding (minor skew is non-breaking)",
                status.api_major,
                status.api_minor,
            );
        }
        Ok(DaemonProbe::Compatible(status))
    }

    /// Raw daemon status probe. Connection absence is `None`; a reachable
    /// daemon's HTTP status and JSON errors are propagated.
    pub(crate) async fn status_optional(&self) -> Result<Option<DaemonStatus>> {
        let Some(response) = self
            .get_optional("/v1/status", "query daemon status")
            .await?
        else {
            return Ok(None);
        };
        Self::parse_status(response).await.map(Some)
    }

    /// Verify the daemon is reachable and speaks this CLI's control API.
    pub(crate) async fn require_compatible(&self) -> Result<DaemonStatus> {
        match self.probe().await? {
            DaemonProbe::Compatible(status) => Ok(status),
            DaemonProbe::Unreachable => Err(anyhow::anyhow!(
                "no daemon answered on the control port at {}",
                self.base
            )),
        }
    }

    /// Daemon runtime facts from a reachable, compatible daemon.
    pub(crate) async fn status(&self) -> Result<DaemonStatus> {
        self.status_optional().await?.ok_or_else(|| {
            anyhow::anyhow!("no daemon answered on the control port at {}", self.base)
        })
    }

    async fn get_optional(
        &self,
        path: &str,
        context: &'static str,
    ) -> Result<Option<reqwest::Response>> {
        match self.http.get(format!("{}{}", self.base, path)).send().await {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.is_connect() || error.is_timeout() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("{context} at {}", self.base)),
        }
    }

    async fn parse_status(response: reqwest::Response) -> Result<DaemonStatus> {
        let response = response
            .error_for_status()
            .context("daemon status request failed")?;
        response.json().await.context("parse daemon status")
    }

    /// Converge the running daemon's mount set to the on-disk desired state
    /// under `mounts/*.json`. Reconcile compiles WASM for added or changed
    /// mounts, so it gets the long mount-load timeout rather than the default.
    pub(crate) async fn reconcile(&self) -> Result<ReconcileReport> {
        let response = self
            .http
            .post(format!("{}/v1/reconcile", self.base))
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .with_context(|| format!("reconcile mounts on daemon at {}", self.base))?
            .error_for_status()
            .context("daemon reconcile request failed")?;
        response.json().await.context("parse reconcile report")
    }

    /// Ask the daemon to unmount its frontend and exit, returning what it tore
    /// down. `None` when no daemon answered, so the caller can fall back to a
    /// stale-mount sweep.
    pub(crate) async fn shutdown(&self) -> Result<Option<StopReport>> {
        match self
            .http
            .post(format!("{}/v1/shutdown", self.base))
            .send()
            .await
        {
            Ok(response) => {
                let report = response
                    .error_for_status()
                    .context("daemon shutdown request failed")?
                    .json()
                    .await
                    .context("parse stop report")?;
                Ok(Some(report))
            },
            Err(error) if error.is_connect() || error.is_timeout() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("shutdown daemon at {}", self.base)),
        }
    }

    /// True once the daemon reports the filesystem is serving.
    pub(crate) async fn ready(&self) -> bool {
        matches!(
            self.http
                .get(format!("{}/v1/ready", self.base))
                .send()
                .await,
            Ok(response) if response.status().is_success()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn status_optional_propagates_reachable_status_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let response = if request.starts_with("GET /v1/status ") {
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n".to_string()
            } else {
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_string()
            };
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = DaemonClient {
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };

        let error = client.status_optional().await.unwrap_err();
        assert!(format!("{error:#}").contains("daemon status request failed"));
        server.await.unwrap();
    }

    /// A daemon reporting a different major must be refused.
    #[tokio::test]
    async fn probe_refuses_on_major_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let _ = String::from_utf8_lossy(&request[..read]);
            let response = json_response(&status_body("old-daemon", API_MAJOR + 1, 0));
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = DaemonClient {
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };

        let err = client.probe().await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("control API"),
            "error should mention control API mismatch: {msg}"
        );
    }

    /// A daemon reporting the same major but a different minor must proceed (with a warning).
    #[tokio::test]
    async fn probe_proceeds_on_minor_skew() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            let _ = String::from_utf8_lossy(&request[..read]);
            let response = json_response(&status_body("newer-daemon", API_MAJOR, API_MINOR + 1));
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = DaemonClient {
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };

        // Minor skew: probe must succeed (return Compatible).
        assert!(matches!(
            client.probe().await.unwrap(),
            DaemonProbe::Compatible(_)
        ));
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn status_body(version: &str, api_major: u16, api_minor: u16) -> String {
        format!(
            r#"{{
                "version":"{version}",
                "api_major":{api_major},
                "api_minor":{api_minor},
                "mount_point":"/tmp/omnifs",
                "config_dir":"/tmp/omnifs-home",
                "cache_dir":"/tmp/omnifs-home/cache",
                "providers_dir":"/tmp/omnifs-home/providers",
                "frontend":null,
                "mounts":[]
            }}"#
        )
    }
}
