//! Loopback HTTP server for the OAuth2/OIDC login callback.
//!
//! Binds a random port on 127.0.0.1 with a fresh per-attempt state value,
//! waits for the relay to redirect the browser back with a matching state
//! and a session token, and serves a simple receipt page.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::sync::SyncError;

/// How often to poll for new connections.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A loopback HTTP server that captures the login callback.
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
    state: String,
}

impl CallbackServer {
    /// Bind a loopback listener on a random port with a fresh state value.
    pub fn bind() -> Result<Self, SyncError> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(SyncError::Io)?;
        let port = listener.local_addr().map_err(SyncError::Io)?.port();
        let state = Uuid::new_v4().simple().to_string();
        Ok(Self { listener, port, state })
    }

    /// The callback URL to pass to the relay, e.g.
    /// `http://127.0.0.1:54321/callback?state=<state>`.
    pub fn callback_url(&self) -> String {
        format!("http://127.0.0.1:{}/callback?state={}", self.port, self.state)
    }

    /// Wait for the relay to redirect the browser back with a matching state
    /// and a session token.
    ///
    /// Single-threaded: polls the listener in a loop until a valid callback
    /// arrives or the timeout expires. Rejected requests do not consume the
    /// attempt. Once a matching callback is accepted, the listener is closed.
    pub fn wait_for_token(self, timeout: Duration) -> Result<String, SyncError> {
        self.listener
            .set_nonblocking(true)
            .map_err(SyncError::Io)?;

        let deadline = Instant::now() + timeout;

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(token) = self.handle_connection(stream) {
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

    /// Handle a single HTTP connection. Returns `Some(token)` if a valid
    /// callback was captured, `None` otherwise (caller should keep accepting).
    fn handle_connection(&self, mut stream: TcpStream) -> Option<String> {
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

        match self.extract_callback(&request_line) {
            Some(token) => {
                write_response(&mut stream, 200, "text/html", RECEIPT_PAGE);
                Some(token)
            }
            None => {
                write_response(&mut stream, 400, "text/plain", "Bad Request");
                None
            }
        }
    }

    /// Validate a request line and extract the session token.
    ///
    /// Accepts only GET requests to the exact target `/callback?...` —
    /// carrying exactly one `state` parameter matching this server's state
    /// and exactly one non-empty `token` parameter.
    fn extract_callback(&self, request_line: &str) -> Option<String> {
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?;
        if method != "GET" {
            return None;
        }
        let target = parts.next()?;
        let raw_path = target.split('?').next().unwrap_or("");
        if raw_path != "/callback" {
            return None;
        }
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{}", target)).ok()?;

        let mut state_count = 0;
        let mut token: Option<String> = None;
        let mut token_count = 0;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "state" => {
                    state_count += 1;
                    if value != self.state {
                        return None;
                    }
                }
                "token" => {
                    token_count += 1;
                    if value.is_empty() {
                        return None;
                    }
                    token = Some(value.into_owned());
                }
                _ => {}
            }
        }

        if state_count != 1 || token_count != 1 {
            return None;
        }
        token
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

/// Page shown in the browser when a valid callback was received.
const RECEIPT_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>todo — login callback received</title>
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
  h1 { font-size: 1.25rem; margin: 0 0 .5rem; font-weight: 600; }
  p { margin: 0; opacity: .7; font-size: .9rem; line-height: 1.5; }
</style>
</head>
<body>
  <div class="card">
    <h1>Login response received</h1>
    <p>The login response was delivered to the todo CLI. Check the terminal to see whether login completed. You can close this tab.</p>
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

    fn valid_request(state: &str, token: &str) -> String {
        format!("GET /callback?state={state}&token={token} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
    }

    #[test]
    fn callback_url_contains_state() {
        let server = CallbackServer::bind().unwrap();
        assert_eq!(
            server.callback_url(),
            format!("http://127.0.0.1:{}/callback?state={}", server.port, server.state)
        );
    }

    #[test]
    fn state_is_fresh_per_attempt() {
        let a = CallbackServer::bind().unwrap();
        let b = CallbackServer::bind().unwrap();
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn valid_callback_is_accepted_once() {
        let server = CallbackServer::bind().unwrap();
        let addr = server.listener.local_addr().unwrap().to_string();
        let req = valid_request(&server.state, "session-token-1");

        std::thread::scope(|s| {
            let client = s.spawn(move || send_request(&addr, &req));
            let token = server.wait_for_token(Duration::from_secs(5)).unwrap();
            assert_eq!(token, "session-token-1");
            let resp = client.join().unwrap();
            assert!(resp.contains("200 OK"));
            assert!(!resp.contains("Logged in"));
        });
    }

    #[test]
    fn wrong_state_rejected_then_correct_completes() {
        let server = CallbackServer::bind().unwrap();
        let addr = server.listener.local_addr().unwrap().to_string();
        let good = valid_request(&server.state, "real-token");

        std::thread::scope(|s| {
            let client = s.spawn(move || {
                let bad = send_request(&addr, "GET /callback?state=wrong&token=attacker HTTP/1.1\r\n\r\n");
                assert!(bad.contains("400 Bad Request"));
                send_request(&addr, &good)
            });
            let token = server.wait_for_token(Duration::from_secs(5)).unwrap();
            assert_eq!(token, "real-token");
            assert!(client.join().unwrap().contains("200 OK"));
        });
    }

    #[test]
    fn foreign_attempt_state_is_rejected() {
        let other = CallbackServer::bind().unwrap();
        let server = CallbackServer::bind().unwrap();
        assert!(server
            .extract_callback(&format!(
                "GET /callback?state={}&token=t HTTP/1.1",
                other.state
            ))
            .is_none());
    }

    #[test]
    fn missing_state_rejected_then_correct_completes() {
        let server = CallbackServer::bind().unwrap();
        let addr = server.listener.local_addr().unwrap().to_string();
        let good = valid_request(&server.state, "t");

        std::thread::scope(|s| {
            s.spawn(move || {
                let bad = send_request(&addr, "GET /callback?token=attacker HTTP/1.1\r\n\r\n");
                assert!(bad.contains("400 Bad Request"));
                send_request(&addr, &good)
            });
            let token = server.wait_for_token(Duration::from_secs(5)).unwrap();
            assert_eq!(token, "t");
        });
    }

    #[test]
    fn wrong_method_path_and_empty_token_rejected() {
        let server = CallbackServer::bind().unwrap();
        assert!(server.extract_callback("POST /callback?state=x&token=y HTTP/1.1").is_none());
        assert!(server.extract_callback("GET /not-the-callback?state=x&token=y HTTP/1.1").is_none());
        assert!(server
            .extract_callback(&format!("GET /other/../callback?state={}&token=t HTTP/1.1", server.state))
            .is_none());
        assert!(server
            .extract_callback(&format!("GET /callback/../callback?state={}&token=t HTTP/1.1", server.state))
            .is_none());
        assert!(server.extract_callback("GET /callback?/x?state=x&token=y HTTP/1.1").is_none());
        assert!(server.extract_callback(&format!("GET /callback?state={}&token= HTTP/1.1", server.state)).is_none());
        assert!(server.extract_callback("GET /callback HTTP/1.1").is_none());
        assert!(server.extract_callback("garbage").is_none());
    }

    #[test]
    fn duplicate_parameters_rejected_in_both_orders() {
        let server = CallbackServer::bind().unwrap();
        let state = &server.state;
        assert!(server
            .extract_callback(&format!("GET /callback?state={state}&state={state}&token=t HTTP/1.1"))
            .is_none());
        assert!(server
            .extract_callback(&format!("GET /callback?token=t&token=t&state={state} HTTP/1.1"))
            .is_none());
        assert!(server
            .extract_callback(&format!("GET /callback?state={state}&token=t&state={state} HTTP/1.1"))
            .is_none());
        assert!(server
            .extract_callback(&format!("GET /callback?token=t&state={state}&token=t2 HTTP/1.1"))
            .is_none());
    }

    #[test]
    fn token_is_url_decoded() {
        let server = CallbackServer::bind().unwrap();
        let token = server
            .extract_callback(&format!(
                "GET /callback?state={}&token=abc%3A123%2Fdef HTTP/1.1",
                server.state
            ))
            .unwrap();
        assert_eq!(token, "abc:123/def");
    }

    #[test]
    fn callback_server_timeout() {
        let server = CallbackServer::bind().unwrap();
        let result = server.wait_for_token(Duration::from_millis(100));
        assert!(matches!(result, Err(SyncError::LoginTimeout)));
    }
}
