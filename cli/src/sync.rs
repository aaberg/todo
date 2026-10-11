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

    let me = confirm_login(relay_url, &token)?;
    save_session(relay_url, &token)?;
    match me.email {
        Some(email) => println!("Logged in as {email}"),
        None => println!("Logged in successfully"),
    }
    Ok(())
}

/// Build the relay login URL with the CLI callback parameter.
fn build_login_url(relay_url: &str, cli_callback: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("{relay_url}/auth/login"))
        .expect("relay URL is valid");
    url.query_pairs_mut()
        .append_pair("cli_callback", cli_callback);
    url.into()
}

/// Persist the session token and relay URL to config.
fn save_session(relay_url: &str, token: &str) -> Result<(), SyncError> {
    let mut config = Config::load();
    config.sync_url = Some(relay_url.to_string());
    config.session_token = Some(token.to_string());
    ensure_device_id(&mut config);
    config.save().map_err(SyncError::Config)
}

/// Query `/me` to confirm the login before persisting credentials.
fn confirm_login(relay_url: &str, token: &str) -> Result<MeResponse, SyncError> {
    let client = reqwest::blocking::Client::new();
    let me_url = format!("{relay_url}/me");

    let resp = client
        .get(&me_url)
        .bearer_auth(token)
        .send()
        .map_err(SyncError::Http)?;
    if !resp.status().is_success() {
        return Err(SyncError::RelayError(resp.status().as_u16()));
    }
    resp.json::<MeResponse>().map_err(SyncError::Http)
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

    // Apply pulled events locally (dedup by event_id via INSERT OR IGNORE).
    // The cursor only advances past successfully applied events; an event we
    // can't parse stops the pull so it is retried (and surfaced) next sync.
    let pulled_count = apply_pulled_events(db, last_pulled, &pull_data.events)?;

    // 2. PUSH: send only local events the relay has not seen yet, in batches.
    //
    // `last_pushed_rowid` is a cursor into the local log's implicit rowid
    // (insertion order, never reused). It is advanced only after a batch is
    // accepted, so a failed push retries the same range next time. The relay
    // dedupes by (user_id, event_id) anyway, so a conservative cursor is safe.
    const PUSH_BATCH_SIZE: usize = 500;

    let mut pushed_rowid: i64 = db
        .get_state("last_pushed_rowid")
        .map_err(SyncError::Db)?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut pushed_count = 0;

    let push_url = format!("{}/push", relay_url);

    loop {
        let batch = db
            .events_since(pushed_rowid, PUSH_BATCH_SIZE)
            .map_err(SyncError::Db)?;
        if batch.is_empty() {
            break;
        }

        let last_rowid = batch.last().unwrap().0;
        let wire_events: Vec<WireEvent> = batch.iter().map(|(_, e)| to_wire(e)).collect();

        let push_resp = client
            .post(&push_url)
            .bearer_auth(token)
            .json(&PushRequest { events: wire_events })
            .send()
            .map_err(SyncError::Http)?;

        if !push_resp.status().is_success() {
            return Err(SyncError::RelayError(push_resp.status().as_u16()));
        }

        let push_data: PushResponse = push_resp.json().map_err(SyncError::Http)?;
        pushed_count += push_data.accepted;

        // Advance the cursor only after the relay confirmed the batch.
        pushed_rowid = last_rowid;
        db.set_state("last_pushed_rowid", &pushed_rowid.to_string())
            .map_err(SyncError::Db)?;

        if batch.len() < PUSH_BATCH_SIZE {
            break;
        }
    }

    Ok(SyncStats {
        pulled: pulled_count,
        pushed: pushed_count,
    })
}

pub struct SyncStats {
    pub pulled: usize,
    pub pushed: usize,
}

/// Apply pulled events to the local log, advancing `last_pulled_seq` only
/// through events that were successfully converted and appended. An event
/// that fails `from_wire` aborts the pass: the cursor is left before it, the
/// error is surfaced, and the event is retried on the next sync.
fn apply_pulled_events(
    db: &mut Database,
    last_pulled: i64,
    pulled_events: &[todo_common::wire::PulledEvent],
) -> Result<usize, SyncError> {
    let mut pulled_count = 0;
    let mut latest_seq = last_pulled;
    for pulled in pulled_events {
        match from_wire(&pulled.event) {
            Ok(event) => {
                db.append_event(&event).map_err(SyncError::Db)?;
                pulled_count += 1;
                latest_seq = pulled.seq;
            }
            Err(e) => {
                db.set_state("last_pulled_seq", &latest_seq.to_string())
                    .map_err(SyncError::Db)?;
                eprintln!(
                    "aborting pull on incompatible event from relay (event_id {}, seq {}) — it will be retried on the next sync: {e}",
                    pulled.event.event_id, pulled.seq
                );
                return Err(e);
            }
        }
    }
    db.set_state("last_pulled_seq", &latest_seq.to_string())
        .map_err(SyncError::Db)?;
    Ok(pulled_count)
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
    fn build_login_url_includes_callback() {
        let url = build_login_url(
            "https://relay.example.com",
            "http://127.0.0.1:8080/callback?state=abc",
        );
        assert_eq!(
            url,
            "https://relay.example.com/auth/login?cli_callback=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback%3Fstate%3Dabc"
        );
    }

    #[test]
    fn build_login_url_round_trips_callback() {
        let callback = "http://127.0.0.1:54321/callback?state=abc";
        let url = build_login_url("https://relay.example.com", callback);
        let parsed = reqwest::Url::parse(&url).unwrap();
        let cli_callback: String = parsed
            .query_pairs()
            .find(|(k, _)| k == "cli_callback")
            .unwrap()
            .1
            .into();
        assert_eq!(cli_callback, callback);
    }

    fn test_database() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn pull_cursor_stops_at_incompatible_event() {
        let mut db = test_database();
        let good = to_wire(&sample_event());
        let mut bad = to_wire(&sample_event());
        bad.event_type = "future_type".to_string();

        let pulled = vec![
            todo_common::wire::PulledEvent { seq: 1, event: good },
            todo_common::wire::PulledEvent { seq: 2, event: bad },
            todo_common::wire::PulledEvent { seq: 3, event: to_wire(&sample_event()) },
        ];

        let result = apply_pulled_events(&mut db, 0, &pulled);
        assert!(result.is_err());

        // Only the first event was applied; the cursor stays before the bad one.
        assert_eq!(db.all_events().unwrap().len(), 1);
        assert_eq!(db.get_state("last_pulled_seq").unwrap().as_deref(), Some("1"));

        // Retrying from the saved cursor re-fetches the bad event first.
        let next = apply_pulled_events(&mut db, 1, &pulled[1..]);
        assert!(next.is_err());
        assert_eq!(db.get_state("last_pulled_seq").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn pull_cursor_advances_on_clean_pass() {
        let mut db = test_database();
        let events = vec![
            todo_common::wire::PulledEvent { seq: 1, event: to_wire(&sample_event()) },
            todo_common::wire::PulledEvent { seq: 2, event: to_wire(&sample_event()) },
        ];

        let count = apply_pulled_events(&mut db, 0, &events).unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.all_events().unwrap().len(), 2);
        assert_eq!(db.get_state("last_pulled_seq").unwrap().as_deref(), Some("2"));
    }
}
