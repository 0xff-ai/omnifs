//! Shared integration-test helpers for `omnifs-engine`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A local fake OAuth token endpoint. `start(true)` makes every refresh return
/// `invalid_grant`; `start(false)` rotates a fresh token and counts refreshes.
#[derive(Clone)]
pub struct FakeTokenServer {
    endpoint: String,
    refreshes: Arc<AtomicUsize>,
}

impl FakeTokenServer {
    pub async fn start(fail: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Self {
            endpoint: format!("http://{addr}/token"),
            refreshes: Arc::new(AtomicUsize::new(0)),
        };
        let task = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let task = task.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 8192];
                    let read = stream.read(&mut buf).await.unwrap();
                    let request = String::from_utf8_lossy(&buf[..read]);
                    assert!(request.starts_with("POST /token "));
                    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                    let params: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    assert_eq!(
                        params.get("grant_type").map(String::as_str),
                        Some("refresh_token")
                    );
                    if fail {
                        let body = r#"{"error":"invalid_grant","error_description":"revoked"}"#;
                        write_response(&mut stream, "400 Bad Request", body).await;
                        return;
                    }
                    let id = task.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = serde_json::json!({
                        "access_token": format!("access-refresh-{id}"),
                        "refresh_token": format!("refresh-rotated-{id}"),
                        "expires_in": 3600,
                        "token_type": "Bearer",
                        "scope": "read",
                    })
                    .to_string();
                    write_response(&mut stream, "200 OK", &body).await;
                });
            }
        });
        server
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}
