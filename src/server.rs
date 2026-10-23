//! A small HTTP/1.1 server for the OAuth redirect callback and the status
//! endpoint.
//!
//! The server only listens on the loopback interface; the comic itself is
//! shown in a native window instead of a web page.

use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A parsed HTTP request.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Request headers, kept for diagnostics.
    #[allow(dead_code)]
    pub headers: HashMap<String, String>,
}

impl HttpRequest {
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|v| v.as_str())
    }
}

/// A response produced by the handler.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub extra_headers: Vec<(String, String)>,
}

impl HttpResponse {
    pub fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            body: body.into().into_bytes(),
            extra_headers: Vec::new(),
        }
    }

    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".to_string(),
            body: body.into().into_bytes(),
            extra_headers: Vec::new(),
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: format!("redirecting to {location}").into_bytes(),
            extra_headers: vec![("Location".to_string(), location.to_string())],
        }
    }

    pub fn method_not_allowed() -> Self {
        Self {
            status: 405,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: b"only GET is supported".to_vec(),
            extra_headers: Vec::new(),
        }
    }

    pub fn not_found(message: &str) -> Self {
        Self {
            status: 404,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: message.as_bytes().to_vec(),
            extra_headers: Vec::new(),
        }
    }

    pub fn error(message: &str) -> Self {
        Self {
            status: 500,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: message.as_bytes().to_vec(),
            extra_headers: Vec::new(),
        }
    }
}

/// Boxed future returned by a request handler.
pub type HandlerFuture = Pin<Box<dyn Future<Output = HttpResponse> + Send>>;

/// Handler used by the server.
pub type Handler = Arc<dyn Fn(HttpRequest) -> HandlerFuture + Send + Sync>;

/// Start the HTTP server on the given listener.
pub async fn serve(listener: TcpListener, handler: Handler) -> Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let _ = peer;
            if let Err(err) = handle_connection(stream, handler).await {
                eprintln!("http: {err}");
            }
        });
    }
}

async fn handle_connection(mut stream: TcpStream, handler: Handler) -> Result<()> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if raw.len() > 64 * 1024 {
            break;
        }
    }

    let request = match parse_request(&raw) {
        Some(request) => request,
        None => {
            let response = HttpResponse::error("malformed request");
            write_response(&mut stream, &response).await?;
            return Ok(());
        }
    };

    let response = handler(request).await;
    write_response(&mut stream, &response).await?;
    Ok(())
}

fn parse_request(raw: &[u8]) -> Option<HttpRequest> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let (path, query_string) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut query = HashMap::new();
    for pair in query_string.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(url_decode(key), url_decode(value));
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    Some(HttpRequest {
        method,
        path,
        query,
        headers,
    })
}

async fn write_response(stream: &mut TcpStream, response: &HttpResponse) -> Result<()> {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await?;
    Ok(())
}

/// Decode percent escapes and `+` in a query value.
pub fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line_and_headers() {
        let raw = b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost:3000\r\n\r\n";
        let request = parse_request(raw).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/callback");
        assert_eq!(request.query("code"), Some("abc"));
        assert_eq!(request.query("state"), Some("xyz"));
    }

    #[test]
    fn decodes_query_values() {
        assert_eq!(url_decode("a+b%2Fc"), "a b/c");
    }
}
