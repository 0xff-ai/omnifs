use crate::error::AuthError;
use oauth2::basic::BasicTokenResponse;
use oauth2::{CsrfToken, RedirectUrl};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

pub(crate) struct LoopbackEndpoint {
    listener: TcpListener,
    redirect_uri: RedirectUrl,
}

impl LoopbackEndpoint {
    pub(crate) async fn bind(template: &str) -> Result<Self, AuthError> {
        let bind_url = Self::url_with_port(template, 0)?;
        let host = bind_url.host_str().ok_or(AuthError::InvalidRedirectUri)?;
        let port = bind_url.port().ok_or(AuthError::InvalidRedirectUri)?;
        let listener = TcpListener::bind(format!("{host}:{port}")).await?;
        let redirect_uri = RedirectUrl::from_url(Self::url_with_port(
            template,
            listener.local_addr()?.port(),
        )?);
        Ok(Self {
            listener,
            redirect_uri,
        })
    }

    pub(crate) fn redirect_uri(&self) -> &RedirectUrl {
        &self.redirect_uri
    }

    pub(crate) fn into_listener(self) -> TcpListener {
        self.listener
    }

    fn url_with_port(template: &str, port: u16) -> Result<Url, AuthError> {
        let raw = if template.contains("{port}") {
            template.replace("{port}", &port.to_string())
        } else {
            template.to_owned()
        };
        let url = Url::parse(&raw)?;
        if url.scheme() != "http" {
            return Err(AuthError::InvalidRedirectUri);
        }
        let host = url.host_str().ok_or(AuthError::InvalidRedirectUri)?;
        if host != "127.0.0.1" && host != "localhost" {
            return Err(AuthError::InvalidRedirectUri);
        }
        if url.port().is_none() {
            return Err(AuthError::InvalidRedirectUri);
        }
        Ok(url)
    }
}

#[derive(Debug)]
pub(crate) struct LoopbackCallback {
    pub(crate) code: String,
    pub(crate) state: CsrfToken,
}

impl LoopbackCallback {
    pub(crate) fn parse(url: &Url) -> Result<Self, AuthError> {
        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                "error_description" => error_description = Some(value.into_owned()),
                _ => {},
            }
        }
        if let Some(error) = error {
            return Err(AuthError::AuthorizationError {
                error,
                description: error_description,
            });
        }
        Ok(Self {
            code: code.ok_or(AuthError::MissingCode)?,
            state: CsrfToken::new(state.ok_or(AuthError::MissingState)?),
        })
    }
}

#[derive(Debug)]
pub(crate) struct ClientSideTokenCallback {
    pub(crate) token: BasicTokenResponse,
    pub(crate) state: CsrfToken,
}

impl ClientSideTokenCallback {
    pub(crate) fn parse(url: &Url) -> Result<Self, AuthError> {
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        let mut has_access_token = false;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "access_token" => has_access_token = true,
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                "error_description" => error_description = Some(value.into_owned()),
                _ => {},
            }
        }
        if let Some(error) = error {
            return Err(AuthError::AuthorizationError {
                error,
                description: error_description,
            });
        }
        if !has_access_token {
            return Err(AuthError::MissingAccessToken);
        }
        let query = url.query().ok_or(AuthError::MissingAccessToken)?;
        let token = serde_urlencoded::from_str(query).map_err(|_| AuthError::InvalidCallback)?;
        Ok(Self {
            token,
            state: CsrfToken::new(state.ok_or(AuthError::MissingState)?),
        })
    }
}

/// Accept one request on the loopback listener and parse its request line into
/// the requested URL. Non-GET methods are rejected with a 405 (a browser
/// redirect only ever issues a GET); the returned stream stays open so the
/// caller can write its own completion response.
pub(crate) async fn accept_callback_request(
    listener: &TcpListener,
) -> Result<(TcpStream, Url), AuthError> {
    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0; 4096];
    let read = stream.read(&mut buf).await?;
    let request = std::str::from_utf8(&buf[..read]).map_err(|_| AuthError::InvalidCallback)?;
    let request_line = request.lines().next().ok_or(AuthError::InvalidCallback)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(AuthError::InvalidCallback)?;
    let target = parts.next().ok_or(AuthError::InvalidCallback)?;
    if method != "GET" {
        write_callback_response(&mut stream, "405 Method Not Allowed", "method not allowed")
            .await?;
        return Err(AuthError::InvalidCallback);
    }
    let url =
        Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| AuthError::InvalidCallback)?;
    Ok((stream, url))
}

/// Acknowledge a parsed callback to the browser: 200 on success, 400 on a
/// parse/authorization failure.
async fn respond_to_callback<T>(
    stream: &mut TcpStream,
    result: &Result<T, AuthError>,
) -> Result<(), AuthError> {
    match result {
        Ok(_) => write_callback_response(stream, "200 OK", "authorization complete").await,
        Err(_) => write_callback_response(stream, "400 Bad Request", "authorization failed").await,
    }
}

pub(crate) async fn read_loopback_callback(
    listener: TcpListener,
) -> Result<LoopbackCallback, AuthError> {
    let (mut stream, url) = accept_callback_request(&listener).await?;
    let callback = LoopbackCallback::parse(&url);
    respond_to_callback(&mut stream, &callback).await?;
    callback
}

pub(crate) async fn read_client_side_callback(
    listener: TcpListener,
) -> Result<ClientSideTokenCallback, AuthError> {
    for _ in 0..3 {
        let (mut stream, url) = accept_callback_request(&listener).await?;
        if url.query().is_some() {
            let callback = ClientSideTokenCallback::parse(&url);
            respond_to_callback(&mut stream, &callback).await?;
            return callback;
        }

        write_fragment_capture_response(&mut stream).await?;
    }
    Err(AuthError::MissingAccessToken)
}

async fn write_fragment_capture_response(stream: &mut TcpStream) -> Result<(), AuthError> {
    const BODY: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>omnifs authorization</title>
<p>Completing authorization...</p>
<script>
const fragment = window.location.hash.startsWith("#") ? window.location.hash.slice(1) : "";
if (fragment) {
  window.location.replace(window.location.pathname + "?" + fragment);
} else {
  document.body.textContent = "Authorization response did not include a token.";
}
</script>
"##;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn write_callback_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}
