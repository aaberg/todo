//! Sync module — talks to the relay over HTTP.
//!
//! Flows:
//! - `login`:  loopback server + browser → OIDC → session token in config
//! - `sync`:   pull remote events → append locally → push local events
//! - `logout`: revoke session on relay + delete local token
//! - `whoami`: query /me to show current user

use std::time::Duration;

use todo_common::wire::*;
use uuid::Uuid;

use crate::{
    callback::CallbackServer,
    config::{ensure_device_id, Config},
    db::Database,
    event::{Event, EventPayload, EventType},
};


// ─── Wire conversion ───

/// Convert a local domain Event to a WireEvent for HTTP transport.
pub fn to_wire(event: &Event) -> WireEvent {
    let (event_type, payload) = match &event.payload {
        EventPayload::Create { description, due_date } => (
            "create".to_string(),
            serde_json::json!({
                "type": "create",
                "description": description,
                "due_date": due_date,
            }),
        ),
        EventPayload::Update {
            description,
            due_date,
            completed_on,
        } => (
            "update".to_string(),
            serde_json::json!({
                "type": "update",
                "description": description,
                "due_date": due_date,
                "completed_on": completed_on,
            }),
        ),
        EventPayload::Delete => (
            "delete".to_string(),
            serde_json::json!({ "type": "delete" }),
        ),
    };
    WireEvent {
        event_id: event.event_id,
        device_id: event.device_id,
        timestamp: event.timestamp,
        todo_uuid: event.todo_uuid,
        event_type,
        payload,
    }
}

/// Convert a WireEvent from HTTP to a local domain Event.
pub fn from_wire(wire: &WireEvent) -> Result<Event, SyncError> {
    let event_type = EventType::from_str(&wire.event_type)
        .ok_or_else(|| SyncError::UnknownEventType(wire.event_type.clone()))?;
    let payload = serde_json::from_value(wire.payload.clone())
        .map_err(SyncError::PayloadParse)?;
    Ok(Event {
        event_id: wire.event_id,
        device_id: wire.device_id,
        timestamp: wire.timestamp,
        todo_uuid: wire.todo_uuid,
        event_type,
        payload,
    })
}

// ─── Login flow ───

/// How long to wait for the browser callback before giving up.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the login flow: start a loopback server, open the browser, wait for
/// the relay to redirect back with a session token.
pub fn login(relay_url: &str) -> Result<(), SyncError> {
    let relay_url = relay_url.trim_end_matches('/');
    let server = CallbackServer::bind()?;
    let login_url = build_login_url(relay_url, &server.callback_url());

    println!("Opening browser for login...");
    println!("If the browser doesn't open, visit:");
    println!("  {login_url}");
    println!();

    // Open the browser (best effort — user can use the printed URL)
    if let Err(e) = open::that(&login_url) {
        eprintln!("failed to open browser: {e}");
    }

    println!("Waiting for login to complete...");
    let token = server.wait_for_token(LOGIN_TIMEOUT)?;

    save_session(relay_url, &token)?;
    confirm_login(relay_url, &token);
    Ok(())
}

/// Build the relay login URL with the CLI callback parameter.
fn build_login_url(relay_url: &str, cli_callback: &str) -> String {
    format!(
        "{relay_url}/auth/login?cli_callback={}",
        url_encode(cli_callback)
    )
}

/// Persist the session token and relay URL to config.
fn save_session(relay_url: &str, token: &str) -> Result<(), SyncError> {
    let mut config = Config::load();
    config.sync_url = Some(relay_url.to_string());
    config.session_token = Some(token.to_string());
    ensure_device_id(&mut config);
    config.save().map_err(SyncError::Config)
}

/// Query `/me` to confirm the login and greet the user.
fn confirm_login(relay_url: &str, token: &str) {
    let client = reqwest::blocking::Client::new();
    let me_url = format!("{relay_url}/me");

    match client.get(&me_url).bearer_auth(token).send() {
        Ok(resp) if resp.status().is_success() => match resp.json::<MeResponse>() {
            Ok(me) => match me.email {
                Some(email) => println!("Logged in as {email}"),
                None => println!("Logged in successfully"),
            },
            Err(e) => {
                eprintln!("failed to parse user info: {e}");
                println!("Logged in successfully");
            }
        },
        Ok(resp) => {
            eprintln!("relay returned HTTP {}", resp.status().as_u16());
            println!("Logged in successfully");
        }
        Err(e) => {
            eprintln!("failed to confirm login: {e}");
            println!("Logged in successfully");
        }
    }
}

/// Percent-encode a string for use in a URL query parameter.
fn url_encode(s: &str) -> String {
    s.replace(':', "%3A")
        .replace('/', "%2F")
        .replace('?', "%3F")
        .replace('&', "%26")
        .replace('=', "%3D")
}

// ─── Sync ───

/// Run a full sync: pull remote events, apply locally, push local events.
pub fn sync(db: &mut Database, config: &Config) -> Result<SyncStats, SyncError> {
    let (relay_url, token) = config.require_auth().map_err(SyncError::Config)?;
    let device_id: Uuid = config
        .device_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(SyncError::MissingDeviceId)?;

    let client = reqwest::blocking::Client::new();
    let relay_url = relay_url.trim_end_matches('/');

    // 1. PULL: get events from other devices
    let last_pulled: i64 = db
        .get_state("last_pulled_seq")
        .map_err(SyncError::Db)?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let pull_url = format!(
        "{}/pull?since={}&exclude_device={}",
        relay_url, last_pulled, device_id
    );

    let pull_resp = client
        .get(&pull_url)
        .bearer_auth(token)
        .send()
        .map_err(SyncError::Http)?;

    if !pull_resp.status().is_success() {
        return Err(SyncError::RelayError(pull_resp.status().as_u16()));
    }

    let pull_data: PullResponse = pull_resp.json().map_err(SyncError::Http)?;

    // Apply pulled events locally (dedup by event_id via INSERT OR IGNORE)
    let mut pulled_count = 0;
    let mut latest_seq = last_pulled;
    for pulled in &pull_data.events {
        if let Ok(event) = from_wire(&pulled.event) {
            db.append_event(&event).map_err(SyncError::Db)?;
            pulled_count += 1;
        }
        latest_seq = latest_seq.max(pulled.seq);
    }
    db.set_state("last_pulled_seq", &latest_seq.to_string())
        .map_err(SyncError::Db)?;

    // 2. PUSH: send all local events to the relay
    let all_events = db.all_events().map_err(SyncError::Db)?;
    let wire_events: Vec<WireEvent> = all_events.iter().map(to_wire).collect();

    let push_url = format!("{}/push", relay_url);
    let push_resp = client
        .post(&push_url)
        .bearer_auth(token)
        .json(&PushRequest {
            events: wire_events,
        })
        .send()
        .map_err(SyncError::Http)?;

    if !push_resp.status().is_success() {
        return Err(SyncError::RelayError(push_resp.status().as_u16()));
    }

    let push_data: PushResponse = push_resp.json().map_err(SyncError::Http)?;

    Ok(SyncStats {
        pulled: pulled_count,
        pushed: push_data.accepted,
    })
}

pub struct SyncStats {
    pub pulled: usize,
    pub pushed: usize,
}

// ─── Logout ───

/// Revoke the session on the relay and delete the local token.
pub fn logout(config: &mut Config) -> Result<(), SyncError> {
    if let (Some(relay_url), Some(token)) = (&config.sync_url, &config.session_token) {
        let client = reqwest::blocking::Client::new();
        let logout_url = format!("{}/auth/logout", relay_url.trim_end_matches('/'));
        let _ = client
            .post(&logout_url)
            .bearer_auth(token)
            .send();
    }
    config.session_token = None;
    config.save().map_err(SyncError::Config)?;
    Ok(())
}

// ─── Whoami ───

/// Query /me and return the user info.
pub fn whoami(config: &Config) -> Result<MeResponse, SyncError> {
    let (relay_url, token) = config.require_auth().map_err(SyncError::Config)?;
    let client = reqwest::blocking::Client::new();
    let me_url = format!("{}/me", relay_url.trim_end_matches('/'));
    let resp = client
        .get(&me_url)
        .bearer_auth(token)
        .send()
        .map_err(SyncError::Http)?;
    if !resp.status().is_success() {
        return Err(SyncError::RelayError(resp.status().as_u16()));
    }
    resp.json().map_err(SyncError::Http)
}

// ─── Errors ───

#[derive(Debug)]
pub enum SyncError {
    Io(std::io::Error),
    Http(reqwest::Error),
    Db(rusqlite::Error),
    Config(crate::config::ConfigError),
    RelayError(u16),
    LoginTimeout,
    MissingDeviceId,
    UnknownEventType(String),
    PayloadParse(serde_json::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "I/O error: {e}"),
            SyncError::Http(e) => write!(f, "HTTP error: {e}"),
            SyncError::Db(e) => write!(f, "database error: {e}"),
            SyncError::Config(e) => write!(f, "{e}"),
            SyncError::RelayError(code) => write!(f, "relay returned HTTP {code}"),
            SyncError::LoginTimeout => write!(f, "login timed out after 5 minutes"),
            SyncError::MissingDeviceId => write!(f, "missing device ID — run `todo login` first"),
            SyncError::UnknownEventType(t) => write!(f, "unknown event type from relay: {t}"),
            SyncError::PayloadParse(e) => write!(f, "failed to parse event payload: {e}"),
        }
    }
}

impl std::error::Error for SyncError {}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventPayload, EventType};
    use chrono::{DateTime, NaiveDate, Utc};

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn sample_event() -> Event {
        Event {
            event_id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            timestamp: DateTime::parse_from_rfc3339("2026-10-07T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            todo_uuid: Uuid::new_v4(),
            event_type: EventType::Create,
            payload: EventPayload::Create {
                description: "Test".to_string(),
                due_date: date("2026-10-08"),
            },
        }
    }

    #[test]
    fn wire_roundtrip_create() {
        let event = sample_event();
        let wire = to_wire(&event);
        let back = from_wire(&wire).unwrap();
        assert_eq!(back.event_id, event.event_id);
        assert_eq!(back.device_id, event.device_id);
        assert_eq!(back.todo_uuid, event.todo_uuid);
        assert_eq!(back.event_type, EventType::Create);
    }

    #[test]
    fn wire_roundtrip_update() {
        let event = Event {
            event_type: EventType::Update,
            payload: EventPayload::Update {
                description: Some("Updated".to_string()),
                due_date: None,
                completed_on: Some(Some(date("2026-10-07"))),
            },
            ..sample_event()
        };
        let wire = to_wire(&event);
        let back = from_wire(&wire).unwrap();
        assert_eq!(back.event_type, EventType::Update);
        if let EventPayload::Update { description, completed_on, .. } = back.payload {
            assert_eq!(description, Some("Updated".to_string()));
            assert_eq!(completed_on, Some(Some(date("2026-10-07"))));
        } else {
            panic!("expected update payload");
        }
    }

    #[test]
    fn wire_roundtrip_delete() {
        let event = Event {
            event_type: EventType::Delete,
            payload: EventPayload::Delete,
            ..sample_event()
        };
        let wire = to_wire(&event);
        let back = from_wire(&wire).unwrap();
        assert_eq!(back.event_type, EventType::Delete);
    }

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("http://127.0.0.1:54321/callback"), "http%3A%2F%2F127.0.0.1%3A54321%2Fcallback");
    }

    #[test]
    fn build_login_url_includes_callback() {
        let url = build_login_url("https://relay.example.com", "http://127.0.0.1:8080/callback");
        assert_eq!(
            url,
            "https://relay.example.com/auth/login?cli_callback=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback"
        );
    }
}
