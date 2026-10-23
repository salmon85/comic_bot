//! A minimal HTTPS client.
//!
//! Only what the Twitch OAuth and Helix APIs need: one request per call, no
//! keep alive, no redirects. Implemented directly on top of `tokio-rustls` so
//! the program stays free of large HTTP frameworks.

use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Parsed response of a HTTPS request.
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).context("failed to parse JSON response")
    }
}

/// HTTPS client with a shared TLS connector.
pub struct HttpsClient {
    connector: TlsConnector,
}

impl HttpsClient {
    pub fn new() -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            roots
                .add(cert)
                .context("failed to add native root certificate")?;
        }
        if roots.is_empty() {
            bail!("no trusted root certificates found on this system");
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
        })
    }

    /// Perform a GET request.
    pub async fn get(&self, url: &str, headers: &[(&str, String)]) -> Result<HttpResponse> {
        self.request("GET", url, headers, None).await
    }

    /// Perform a POST request with a `application/x-www-form-urlencoded` body.
    pub async fn post_form(
        &self,
        url: &str,
        form: &[(&str, String)],
        headers: &[(&str, String)],
    ) -> Result<HttpResponse> {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
            .collect::<Vec<String>>()
            .join("&");
        let mut all_headers: Vec<(&str, String)> = headers.to_vec();
        all_headers.push((
            "Content-Type",
            "application/x-www-form-urlencoded".to_string(),
        ));
        self.request(
            "POST",
            url,
            &all_headers,
            Some(body.into_bytes()),
        )
        .await
    }

    /// Perform an arbitrary HTTPS request.
    pub async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        let parts = UrlParts::parse(url)?;
        let host = parts.host.clone();
        let port = parts.port;
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("failed to connect to {host}:{port}"))?;
        tcp.set_nodelay(true).ok();
        let server_name = rustls_pki_types::ServerName::try_from(host.clone())
            .with_context(|| format!("invalid host name {host}"))?;
        let mut stream = self
            .connector
            .connect(server_name, tcp)
            .await
            .with_context(|| format!("TLS handshake with {host} failed"))?;

        let mut request = format!(
            "{method} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: comicbot/0.1\r\nConnection: close\r\n",
            parts.path_and_query
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = &body {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");

        stream
            .write_all(request.as_bytes())
            .await
            .context("failed to send request")?;
        if let Some(body) = &body {
            stream.write_all(body).await.context("failed to send body")?;
        }
        stream.flush().await.ok();

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .context("failed to read response")?;

        parse_response(&raw)
    }
}

/// Split a raw HTTP response into status and body.
fn parse_response(raw: &[u8]) -> Result<HttpResponse> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed status line: {status_line}"))?;

    // When the server sends a content length only part of the buffer may be
    // present if the connection was closed early; keep what we have.
    let mut body = body;
    if let Some(len) = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    }) {
        if body.len() > len {
            body.truncate(len);
        }
    }

    Ok(HttpResponse { status, body })
}

/// Percent encode a value for form bodies.
pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The pieces of a URL.
struct UrlParts {
    host: String,
    port: u16,
    path_and_query: String,
}

impl UrlParts {
    fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| anyhow::anyhow!("only https URLs are supported: {url}"))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse::<u16>()
                    .with_context(|| format!("invalid port in {url}"))?,
            ),
            None => (authority.to_string(), 443u16),
        };
        Ok(Self {
            host,
            port,
            path_and_query: path.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encoding_escapes_reserved() {
        assert_eq!(urlencode("a b&c=d"), "a+b%26c%3Dd");
    }

    #[test]
    fn parses_urls() {
        let parts = UrlParts::parse("https://id.twitch.tv/oauth2/token?a=1").unwrap();
        assert_eq!(parts.host, "id.twitch.tv");
        assert_eq!(parts.port, 443);
        assert_eq!(parts.path_and_query, "/oauth2/token?a=1");
    }

    #[test]
    fn parses_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.text(), "{}");
    }
}
