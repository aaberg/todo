//! Regression tests for the validate-before-save login sequence.
//!
//! `login` must confirm the session token against the relay's `/me` before
//! persisting anything: an HTTP error, malformed user-info JSON, or a
//! transport failure must fail login without writing a new config and
//! without touching a pre-existing one.
//!
//! Each test spawns the real CLI binary (as an isolated subprocess with its
//! own HOME) and a stub relay, then delivers a callback that matches the
//! CLI's per-attempt state.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// Response modes the stub relay serves for `GET /me`.
enum MeResponse {
    Ok(String),
    Unauthorized,
    MalformedJson,
}

/// A stub relay: serves `/me` according to `mode`, and records the
/// Authorization header of the last request.
fn spawn_stub_relay(mode: MeResponse) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = std::thread::spawn(move || {
        let mut auth = String::new();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            if request_line.starts_with("GET /me ") {
                // drain headers, capture Authorization
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.strip_prefix("Authorization: Bearer ") {
                        auth = v.trim().to_string();
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let (status, body): (&str, &str) = match &mode {
                    MeResponse::Ok(email) => (
                        "200 OK",
                        &format!(
                            "{{\"user_id\": \"00000000-0000-0000-0000-000000000001\", \"email\": \"{email}\"}}"
                        ),
                    ),
                    MeResponse::Unauthorized => ("401 Unauthorized", ""),
                    MeResponse::MalformedJson => ("200 OK", "{not json"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                return auth;
            } else {
                // Not /me — close without a response.
                return auth;
            }
        }
        auth
    });
    (addr, handle)
}

/// Deliver a callback with a matching state and the given token to the CLI's
/// loopback listener, once we've learned the state from the CLI's output.
fn deliver_callback(login_url: &str, token: &str) {
    let callback = login_url
        .split("cli_callback=")
        .nth(1)
        .unwrap()
        .split(char::is_whitespace)
        .next()
        .unwrap();
    let callback = percent_decode(callback);
    let callback = if callback.contains("token=") {
        callback
    } else {
        format!("{}&token={}", callback, token)
    };
    let target = callback
        .split_once("127.0.0.1")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path_query)| format!("/{}", path_query))
        .unwrap();
    let port: &str = callback
        .rsplit_once(':')
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port.parse().unwrap())).unwrap();
    stream
        .write_all(format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", target).as_bytes())
        .unwrap();
    // Best-effort read: the CLI closes the listener immediately after a
    // matching callback, which can reset the connection before we drain it.
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    assert!(
        body.is_empty() || body.starts_with("HTTP/1.1 200"),
        "callback rejected: {body}"
    );
}

fn percent_decode(s: &str) -> String {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Create an isolated HOME with a pre-existing config, run `todo login`
/// against the stub relay, and wait for it to exit. Returns (exit_ok,
/// combined_output).
fn run_login_with_stub(home: &std::path::Path, mode: MeResponse, token: &str) -> (bool, String) {
    let (relay_addr, _me_auth) = spawn_stub_relay(mode);

    // Give the stub relay a moment to bind.
    std::thread::sleep(std::time::Duration::from_millis(100));

    let mut child = Command::new(env!("CARGO_BIN_EXE_todo"))
        .args(["login", "--relay", &relay_addr])
        .env("HOME", home)
        .env("BROWSER", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (login_url, stdout_rx) = read_login_url(&mut child);

    deliver_callback(&login_url, token);

    let output = child.wait_with_output().unwrap();
    let stdout_text = stdout_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_default();
    let combined = format!(
        "{}{}",
        stdout_text,
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), combined)
}

/// Read the CLI's stdout until it prints the relay login URL. Keeps draining
/// stdout in a background thread (so the CLI never sees a broken pipe) and
/// returns a handle to the full captured output once the CLI exits.
fn read_login_url(child: &mut std::process::Child) -> (String, std::sync::mpsc::Receiver<String>) {
    let stdout = child.stdout.take().unwrap();
    let (url_tx, url_rx) = std::sync::mpsc::channel();
    let (full_tx, full_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut full = String::new();
        let mut url_seen = false;
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if !url_seen && line.contains("/auth/login?cli_callback=") {
                        url_seen = true;
                        let _ = url_tx.send(line.trim().to_string());
                    }
                    full.push_str(&line);
                    full.push('\n');
                }
                Err(_) => break,
            }
        }
        let _ = full_tx.send(full);
    });
    let url = url_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("CLI never printed a login URL");
    (url, full_rx)
}

fn config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".todo").join("config.toml")
}

fn temp_home() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "todo-login-test-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn me_401_fails_login_without_writing_fresh_config() {
    let home = temp_home();
    let (ok, output) = run_login_with_stub(&home, MeResponse::Unauthorized, "invalid-token");
    assert!(!ok, "login must fail on /me 401");
    assert!(output.contains("relay returned HTTP 401"), "{output}");
    assert!(
        !config_path(&home).exists(),
        "no config may be written when confirmation fails"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn malformed_me_json_fails_login_without_writing_fresh_config() {
    let home = temp_home();
    let (ok, _output) = run_login_with_stub(&home, MeResponse::MalformedJson, "some-token");
    assert!(!ok, "login must fail on malformed /me JSON");
    assert!(
        !config_path(&home).exists(),
        "no config may be written when confirmation fails"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn transport_failure_fails_login_without_writing_fresh_config() {
    // Stub relay that accepts the connection then drops it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let acceptor = std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });

    let home = temp_home();
    let mut child = Command::new(env!("CARGO_BIN_EXE_todo"))
        .args(["login", "--relay", &addr])
        .env("HOME", &home)
        .env("BROWSER", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (login_url, _stdout_rx) = read_login_url(&mut child);
    deliver_callback(&login_url, "some-token");
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "login must fail on transport error: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = output;
    assert!(
        !config_path(&home).exists(),
        "no config may be written when confirmation fails"
    );
    drop(acceptor);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn me_failure_leaves_pre_existing_config_unchanged() {
    let home = temp_home();
    let config_dir = home.join(".todo");
    std::fs::create_dir_all(&config_dir).unwrap();
    let prior = "sync_url = \"https://old.example.com\"\nsession_token = \"old-token\"\ndevice_id = \"old-device\"\n";
    std::fs::write(config_path(&home), prior).unwrap();

    let (ok, output) = run_login_with_stub(&home, MeResponse::Unauthorized, "attacker-token");
    assert!(!ok, "login must fail on /me 401");
    assert!(output.contains("relay returned HTTP 401"), "{output}");

    let after = std::fs::read_to_string(config_path(&home)).unwrap();
    assert_eq!(
        after, prior,
        "a failed confirmation must leave the prior config byte-identical"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn successful_me_persists_session() {
    let home = temp_home();
    let (ok, output) = run_login_with_stub(&home, MeResponse::Ok("user@example.com".into()), "good-token");
    assert!(ok, "login must succeed: {output}");
    assert!(output.contains("Logged in as user@example.com"), "{output}");

    let config = std::fs::read_to_string(config_path(&home)).unwrap();
    assert!(config.contains("good-token"), "{config}");
    let _ = std::fs::remove_dir_all(&home);
}
