//! HTTP client for the daemon control API.
//!
//! The daemon listens on the container's published loopback port (or its
//! own loopback when running natively). `OMNIFS_DAEMON_ADDR` overrides the
//! `host:port` for non-default setups.

use anyhow::{Context as _, Result};
use omnifs_api::{API_VERSION, DaemonStatus, ReconcileReport, StopReport, VersionInfo};
use std::time::Duration;

use crate::inspector::daemon_addr;

pub(crate) struct DaemonClient {
    base: String,
    http: reqwest::Client,
}

pub(crate) enum DaemonProbe {
    Unreachable,
    Compatible(VersionInfo),
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
    pub(crate) async fn probe(&self) -> Result<DaemonProbe> {
        let Some(info) = self.version().await? else {
            return Ok(DaemonProbe::Unreachable);
        };
        anyhow::ensure!(
            info.api_version == API_VERSION,
            "daemon speaks control API v{}, this CLI speaks v{API_VERSION}; \
             upgrade so the CLI and runtime image versions match (daemon v{})",
            info.api_version,
            info.version,
        );
        Ok(DaemonProbe::Compatible(info))
    }

    /// Raw daemon version probe. This intentionally does not enforce control
    /// API compatibility so launch can distinguish upgrade boundaries from
    /// absence.
    pub(crate) async fn version(&self) -> Result<Option<VersionInfo>> {
        let Some(response) = self
            .get_optional("/v1/version", "query daemon version")
            .await?
        else {
            return Ok(None);
        };
        let info = response
            .error_for_status()
            .context("daemon version request failed")?
            .json()
            .await
            .context("parse daemon version")?;
        Ok(Some(info))
    }

    /// Verify the daemon is reachable and speaks this CLI's control API.
    pub(crate) async fn require_compatible(&self) -> Result<VersionInfo> {
        match self.probe().await? {
            DaemonProbe::Compatible(info) => Ok(info),
            DaemonProbe::Unreachable => Err(anyhow::anyhow!(
                "no daemon answered on the control port at {}",
                self.base
            )),
        }
    }

    /// Daemon runtime facts from a reachable, compatible daemon.
    pub(crate) async fn status(&self) -> Result<DaemonStatus> {
        let response = self
            .get_optional("/v1/status", "query daemon status")
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("no daemon answered on the control port at {}", self.base)
            })?;
        Self::parse_status(response).await
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
    async fn status_propagates_reachable_status_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let response = if request.starts_with("GET /v1/version ") {
                    json_response(&format!(
                        r#"{{"version":"test-daemon","api_version":{API_VERSION}}}"#
                    ))
                } else if request.starts_with("GET /v1/status ") {
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_string()
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = DaemonClient {
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };

        assert!(matches!(
            client.probe().await.unwrap(),
            DaemonProbe::Compatible(_)
        ));
        let error = client.status().await.unwrap_err();
        assert!(format!("{error:#}").contains("daemon status request failed"));
        server.await.unwrap();
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }
}
