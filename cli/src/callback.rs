//! Loopback HTTP server for the OAuth2/OIDC login callback.
//!
//! Binds a random port on 127.0.0.1, waits for the relay to redirect the
//! browser back with a session token, and serves a simple success page.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

use crate::sync::SyncError;

/// How often to poll for new connections.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A loopback HTTP server that captures the login callback.
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
}

impl CallbackServer {
    /// Bind a loopback listener on a random port.
    pub fn bind() -> Result<Self, SyncError> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(SyncError::Io)?;
        let port = listener.local_addr().map_err(SyncError::Io)?.port();
        Ok(Self { listener, port })
    }

    /// The callback URL to pass to the relay, e.g. `http://127.0.0.1:54321/callback`.
    pub fn callback_url(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Wait for the relay to redirect the browser back with a session token.
    ///
    /// Single-threaded: polls the listener in a loop until a token arrives
    /// or the timeout expires. No threads spawned, no leaks.
    pub fn wait_for_token(&self, timeout: Duration) -> Result<String, SyncError> {
        self.listener
            .set_nonblocking(true)
            .map_err(SyncError::Io)?;

        let deadline = Instant::now() + timeout;

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(token) = handle_connection(stream) {
                        return Ok(token);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(SyncError::LoginTimeout);
                    }
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                }
                Err(e) => {
                    eprintln!("callback server: accept error: {e}");
                    if Instant::now() >= deadline {
                        return Err(SyncError::LoginTimeout);
                    }
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                }
            }
        }
    }
}

/// Handle a single HTTP connection. Returns `Some(token)` if a token was
/// captured, `None` otherwise (caller should keep accepting).
fn handle_connection(mut stream: TcpStream) -> Option<String> {
    let reader = match stream.try_clone() {
        Ok(s) => BufReader::new(s),
        Err(e) => {
            eprintln!("callback server: clone stream error: {e}");
            let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return None;
        }
    };

    let mut reader = reader;
    let mut request_line = String::new();
    if let Err(e) = reader.read_line(&mut request_line) {
        eprintln!("callback server: read request error: {e}");
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        return None;
    }

    match extract_token(&request_line) {
        Some(token) => {
            write_response(&mut stream, 200, "text/html", SUCCESS_PAGE);
            Some(token)
        }
        None => {
            write_response(&mut stream, 400, "text/plain", "Bad Request");
            None
        }
    }
}

/// Write a minimal HTTP response.
fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    if let Err(e) = stream.write_all(response.as_bytes()) {
        eprintln!("callback server: write response error: {e}");
    }
    let _ = stream.flush();
}

/// Extract the `token` query parameter from an HTTP request line.
/// E.g. "GET /callback?token=abc123 HTTP/1.1" → Some("abc123")
fn extract_token(request_line: &str) -> Option<String> {
    let path = request_line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "token" {
                return Some(url_decode(value));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    s.replace("%3A", ":")
        .replace("%2F", "/")
        .replace("%3F", "?")
        .replace("%26", "&")
        .replace("%3D", "=")
        .replace("%2B", "+")
        .replace("%20", " ")
}

/// Success page shown in the browser after login completes.
const SUCCESS_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>todo — logged in</title>
<style>
  :root { color-scheme: light dark; }
  body {
    font-family: system-ui, -apple-system, sans-serif;
    display: flex; align-items: center; justify-content: center;
    min-height: 100vh; margin: 0;
    background: #f5f5f5; color: #1a1a1a;
  }
  @media (prefers-color-scheme: dark) {
    body { background: #1a1a1a; color: #f5f5f5; }
  }
  .card {
    text-align: center; padding: 3rem 2.5rem;
    background: #fff; border-radius: 12px;
    box-shadow: 0 4px 24px rgba(0,0,0,.08);
    max-width: 24rem;
  }
  @media (prefers-color-scheme: dark) {
    .card { background: #2a2a2a; box-shadow: 0 4px 24px rgba(0,0,0,.3); }
  }
  .check {
    width: 56px; height: 56px; margin: 0 auto 1.25rem;
    border-radius: 50%; background: #16a34a;
    display: flex; align-items: center; justify-content: center;
  }
  .check svg { width: 28px; height: 28px; stroke: #fff; }
  h1 { font-size: 1.25rem; margin: 0 0 .5rem; font-weight: 600; }
  p { margin: 0; opacity: .7; font-size: .9rem; line-height: 1.5; }
</style>
</head>
<body>
  <div class="card">
    <div class="check">
      <svg viewBox="0 0 24 24" fill="none" stroke-width="3" stroke-linecap="round" stroke-linejoin="round">
        <polyline points="20 6 9 17 4 12"></polyline>
      </svg>
    </div>
    <h1>Logged in</h1>
    <p>You can close this tab and return to the terminal.</p>
  </div>
</body>
</html>"#;

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Helper: send an HTTP request to the callback server and return the response.
    fn send_request(addr: &str, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        buf
    }

    #[test]
    fn extract_token_simple() {
        let line = "GET /callback?token=abc123 HTTP/1.1";
        assert_eq!(extract_token(line), Some("abc123".to_string()));
    }

    #[test]
    fn extract_token_missing() {
        let line = "GET /callback HTTP/1.1";
        assert_eq!(extract_token(line), None);
    }

    #[test]
    fn extract_token_among_other_params() {
        let line = "GET /callback?state=xyz&token=secret&foo=bar HTTP/1.1";
        assert_eq!(extract_token(line), Some("secret".to_string()));
    }

    #[test]
    fn extract_token_url_decoded() {
        let line = "GET /callback?token=abc%3A123%2Fdef HTTP/1.1";
        assert_eq!(extract_token(line), Some("abc:123/def".to_string()));
    }

    #[test]
    fn extract_token_empty_value() {
        let line = "GET /callback?token= HTTP/1.1";
        assert_eq!(extract_token(line), Some("".to_string()));
    }

    #[test]
    fn callback_server_captures_token() {
        let server = CallbackServer::bind().unwrap();
        let addr = server.listener.local_addr().unwrap().to_string();

        let handle = std::thread::spawn(move || {
            send_request(
                &addr,
                "GET /callback?token=test-token-123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
        });

        let token = server.wait_for_token(Duration::from_secs(5)).unwrap();
        assert_eq!(token, "test-token-123");

        let response = handle.join().unwrap();
        assert!(response.contains("200 OK"));
        assert!(response.contains("Logged in"));
    }

    #[test]
    fn callback_server_rejects_missing_token_then_accepts() {
        let server = CallbackServer::bind().unwrap();
        let addr = server.listener.local_addr().unwrap().to_string();

        // Run both requests and the server in a scoped thread so they
        // run concurrently. The server processes the 400 request first,
        // keeps running, then captures the token from the second request.
        std::thread::scope(|s| {
            let client = s.spawn(move || {
                // First: no token → 400
                let resp = send_request(&addr, "GET /callback HTTP/1.1\r\n\r\n");
                assert!(resp.contains("400 Bad Request"));
                // Second: valid token → 200
                send_request(
                    &addr,
                    "GET /callback?token=valid-token HTTP/1.1\r\n\r\n",
                )
            });

            let token = server.wait_for_token(Duration::from_secs(5)).unwrap();
            assert_eq!(token, "valid-token");

            let resp = client.join().unwrap();
            assert!(resp.contains("200 OK"));
        });
    }

    #[test]
    fn callback_server_timeout() {
        let server = CallbackServer::bind().unwrap();
        let result = server.wait_for_token(Duration::from_millis(100));
        assert!(matches!(result, Err(SyncError::LoginTimeout)));
    }

    #[test]
    fn callback_url_format() {
        let server = CallbackServer::bind().unwrap();
        assert_eq!(
            server.callback_url(),
            format!("http://127.0.0.1:{}/callback", server.port)
        );
    }
}
