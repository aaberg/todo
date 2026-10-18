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
    config::{Config, ensure_device_id},
    db::{Database, SyncBinding, SyncBindingError, SyncBindingState},
    event::{Event, EventPayload, EventType},
};

// ─── Wire conversion ───

/// Convert a local domain Event to a WireEvent for HTTP transport.
pub fn to_wire(event: &Event) -> WireEvent {
    let (event_type, payload) = match &event.payload {
        EventPayload::Create {
            description,
            due_date,
        } => (
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
        } => {
            // Omit completed_on when the outer option is None so the wire
            // payload preserves the None = no change / null = mark undone
            // distinction (null here would deserialize as Some(None)).
            let mut payload = serde_json::json!({
                "type": "update",
                "description": description,
                "due_date": due_date,
            });
            if let Some(inner) = completed_on {
                payload["completed_on"] = serde_json::json!(inner);
            }
            ("update".to_string(), payload)
        }
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
    let payload = serde_json::from_value(wire.payload.clone()).map_err(SyncError::PayloadParse)?;
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
/// the relay to redirect back with a session token, verify the session
/// identity, and only then persist credentials — after checking the
/// dataset's immutable sync binding.
pub fn login(db: &mut Database, relay_url: &str) -> Result<(), SyncError> {
    let relay_url = canonical_relay_url(relay_url)?;

    // An obviously different relay can be rejected before opening the browser.
    match db.sync_binding().map_err(SyncError::Db)? {
        SyncBindingState::Bound(binding) if binding.relay_url != relay_url => {
            return Err(SyncError::Binding(SyncBindingError::Mismatch(binding)));
        }
        SyncBindingState::Corrupt => {
            return Err(SyncError::Binding(SyncBindingError::Corrupt));
        }
        _ => {}
    }

    let server = CallbackServer::bind()?;
    let login_url = build_login_url(&relay_url, &server.callback_url())?;

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

    let me = verify_identity(&relay_url, &token)?;
    verify_or_bind_on_login(db, &relay_url, me.user_id)?;
    save_session(&relay_url, &token)?;
    match me.email {
        Some(email) => println!("Logged in as {email}"),
        None => println!("Logged in successfully"),
    }
    Ok(())
}

/// Check the candidate session identity against the dataset binding before
/// credentials are persisted. For an already-bound dataset this is an exact
/// match-or-fail. For an unbound dataset, any evidence of a prior configured
/// sync relationship (cursor keys or stored credentials — an older CLI could
/// log in without ever completing a sync) makes it a legacy upgrade: the
/// previously configured session — if still verifiable — establishes the
/// binding first, and the candidate must match it. Without evidence it is a
/// genuinely unbound dataset and the candidate establishes the first binding.
/// If legacy evidence exists but cannot be verified, fail closed.
fn verify_or_bind_on_login(
    db: &mut Database,
    relay_url: &str,
    user_id: Uuid,
) -> Result<(), SyncError> {
    match db.sync_binding().map_err(SyncError::Db)? {
        SyncBindingState::Bound(_) | SyncBindingState::Corrupt => {
            db.bind_or_verify_sync(relay_url, user_id)
                .map_err(SyncError::Binding)?;
            Ok(())
        }
        SyncBindingState::Absent => {
            let config = Config::load();
            if !has_prior_sync_relationship(db, &config)? {
                // Genuinely unbound, never-configured dataset: first binding.
                // Offline-created events are retained and will sync.
                db.bind_or_verify_sync(relay_url, user_id)
                    .map_err(SyncError::Binding)?;
                return Ok(());
            }
            // Legacy dataset: ownership must come from the previously
            // configured session, not the new token. The candidate must
            // match that verified identity (and relay) before anything is
            // adopted — a rejected login must not mutate the binding or
            // cursor, so the comparison happens before the adoption.
            if let (Some(config_url), Some(token)) = (&config.sync_url, &config.session_token)
                && let Ok(config_relay) = canonical_relay_url(config_url)
                && let Ok(existing_me) = verify_identity(&config_relay, token)
            {
                if config_relay != relay_url || existing_me.user_id != user_id {
                    return Err(SyncError::Binding(SyncBindingError::Mismatch(
                        SyncBinding {
                            relay_url: config_relay,
                            user_id: existing_me.user_id,
                        },
                    )));
                }
                db.adopt_legacy_sync_binding(&config_relay, existing_me.user_id)
                    .map_err(SyncError::Binding)?;
                return Ok(());
            }
            Err(SyncError::LegacyMigrationRequired)
        }
    }
}

/// Whether an unbound dataset shows evidence of a prior configured sync
/// relationship. Cursor keys in the database are one form; stored relay
/// credentials in the config file are another — an older CLI could complete a
/// login without ever syncing, leaving credentials but no cursors. Either
/// means ownership cannot be established from a new candidate token alone.
fn has_prior_sync_relationship(db: &Database, config: &Config) -> Result<bool, SyncError> {
    Ok(db.has_legacy_sync_evidence().map_err(SyncError::Db)?
        || config.session_token.is_some())
}

/// Build the relay login URL with the CLI callback parameter.
/// Rejects relay URLs that are not valid HTTP(S) URLs with a host so that
/// bad `--relay` values or prompted input surface as an error, not a panic.
fn build_login_url(relay_url: &str, cli_callback: &str) -> Result<String, SyncError> {
    let base = canonical_relay_url(relay_url)?;
    let mut url = reqwest::Url::parse(&format!("{base}/auth/login"))
        .map_err(|_| SyncError::InvalidRelayUrl)?;
    url.query_pairs_mut()
        .append_pair("cli_callback", cli_callback);
    Ok(url.into())
}

/// Check that a relay URL is an absolute HTTP(S) URL with a host, and
/// normalize it for binding identity comparison: scheme, host, effective
/// port, and deployment base path, with trailing slashes treated
/// consistently. A port equal to the scheme default is dropped, so
/// `http://h:80` and `http://h` compare equal. Equivalent trailing slashes
/// compare equal; everything else (different host, port, path, or an
/// HTTP-to-HTTPS change) is a different relay.
fn canonical_relay_url(relay_url: &str) -> Result<String, SyncError> {
    let trimmed = relay_url.trim();
    if trimmed.is_empty() {
        return Err(SyncError::InvalidRelayUrl);
    }
    let url = reqwest::Url::parse(trimmed).map_err(|_| SyncError::InvalidRelayUrl)?;
    match url.scheme() {
        "http" | "https" => (),
        _ => return Err(SyncError::InvalidRelayUrl),
    }
    let host = url.host_str().ok_or(SyncError::InvalidRelayUrl)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SyncError::InvalidRelayUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SyncError::InvalidRelayUrl);
    }
    let port = match url.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let base_path = url.path().trim_end_matches('/');
    Ok(format!("{}://{}{}{}", url.scheme(), host, port, base_path))
}

/// Persist the session token and relay URL to config.
fn save_session(relay_url: &str, token: &str) -> Result<(), SyncError> {
    let mut config = Config::load();
    config.sync_url = Some(relay_url.to_string());
    config.session_token = Some(token.to_string());
    ensure_device_id(&mut config);
    config.save().map_err(SyncError::Config)
}

/// Query authenticated `/me` and require a successful, well-formed response.
/// The returned `user_id` is the verified identity used for the sync binding.
fn verify_identity(relay_url: &str, token: &str) -> Result<MeResponse, SyncError> {
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
    let relay_url = canonical_relay_url(relay_url)?;

    // Enforce the dataset binding before any pull, push, cursor change, or
    // network request. Checking the binding's relay URL locally first also
    // means a substituted relay in config.toml is rejected before the active
    // token is sent anywhere.
    match db.sync_binding().map_err(SyncError::Db)? {
        SyncBindingState::Corrupt => {
            return Err(SyncError::Binding(SyncBindingError::Corrupt));
        }
        SyncBindingState::Bound(binding) if binding.relay_url != relay_url => {
            return Err(SyncError::Binding(SyncBindingError::Mismatch(binding)));
        }
        _ => {}
    }

    // Re-verify the identity on every sync: stored credentials can be stale,
    // copied, or manually edited.
    let me = verify_identity(&relay_url, token)?;
    match db.sync_binding().map_err(SyncError::Db)? {
        // Recheck the full pair, not just the user: a concurrent invocation
        // could have bound this previously absent dataset to the same user on
        // a different relay while we were verifying against this one.
        SyncBindingState::Bound(binding)
            if binding.user_id != me.user_id || binding.relay_url != relay_url =>
        {
            return Err(SyncError::Binding(SyncBindingError::Mismatch(binding)));
        }
        SyncBindingState::Bound(_) => {}
        SyncBindingState::Absent => {
            // Unbound dataset. The /me identity verified above comes from the
            // configured session itself (there is no separate candidate
            // token on this path), so binding it is always adopting the
            // verified configured identity — never a guess. With cursor
            // evidence it is a legacy upgrade: adopt the identity and reset
            // the previously unscoped cursor once.
            if db.has_legacy_sync_evidence().map_err(SyncError::Db)? {
                db.adopt_legacy_sync_binding(&relay_url, me.user_id)
                    .map_err(SyncError::Binding)?;
            } else {
                db.bind_or_verify_sync(&relay_url, me.user_id)
                    .map_err(SyncError::Binding)?;
            }
        }
        SyncBindingState::Corrupt => {
            return Err(SyncError::Binding(SyncBindingError::Corrupt));
        }
    }

    // Prefer the config's device_id (the single source of truth, adopted into
    // the DB at startup); fall back to the DB's identity if the config has no
    // valid UUID. Either way this matches the device_id stamped on locally
    // emitted events, so a device never pulls back its own events.
    let device_id: Uuid = config
        .device_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or(db.device_id().map_err(SyncError::Db)?);

    let client = reqwest::blocking::Client::new();

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
            .json(&PushRequest {
                events: wire_events,
            })
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
                if db.append_event(&event).map_err(SyncError::Db)? {
                    pulled_count += 1;
                }
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
        let _ = client.post(&logout_url).bearer_auth(token).send();
    }
    config.session_token = None;
    config.save().map_err(SyncError::Config)?;
    Ok(())
}

// ─── Whoami ───

/// Query /me and return the user info.
pub fn whoami(config: &Config) -> Result<MeResponse, SyncError> {
    let (relay_url, token) = config.require_auth().map_err(SyncError::Config)?;
    whoami_with_url_and_token(relay_url, token)
}

/// Query /me with an explicit relay URL and token, using a short timeout so
/// callers probing connectivity (e.g. the TUI status line) stay responsive.
pub fn whoami_with_url_and_token(
    relay_url: &str,
    token: &str,
) -> Result<MeResponse, SyncError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(SyncError::Http)?;
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
    UnknownEventType(String),
    PayloadParse(serde_json::Error),
    InvalidRelayUrl,
    Binding(SyncBindingError),
    LegacyMigrationRequired,
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
            SyncError::UnknownEventType(t) => write!(f, "unknown event type from relay: {t}"),
            SyncError::PayloadParse(e) => write!(f, "failed to parse event payload: {e}"),
            SyncError::InvalidRelayUrl => write!(
                f,
                "invalid relay URL — expected an HTTP(S) URL with a host, no query, fragment, or embedded credentials, e.g. https://relay.example.com"
            ),
            SyncError::Binding(e) => write!(f, "{e}"),
            SyncError::LegacyMigrationRequired => write!(
                f,
                "this todo database has synced before but does not record which \
                 account owned its history, and the previous credentials cannot be \
                 verified. All local data is kept and local commands remain usable, \
                 but logging in or syncing with a new account would mix two \
                 accounts' histories. Sync once with the original account's valid \
                 session to record it as this database's owner"
            ),
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
        if let EventPayload::Update {
            description,
            completed_on,
            ..
        } = back.payload
        {
            assert_eq!(description, Some("Updated".to_string()));
            assert_eq!(completed_on, Some(Some(date("2026-10-07"))));
        } else {
            panic!("expected update payload");
        }
    }

    #[test]
    fn wire_roundtrip_update_no_completion_change() {
        // Outer None = no change: must not become a mark-undone event after
        // a wire roundtrip.
        let event = Event {
            event_type: EventType::Update,
            payload: EventPayload::Update {
                description: Some("Edited".to_string()),
                due_date: None,
                completed_on: None,
            },
            ..sample_event()
        };
        let wire = to_wire(&event);
        let back = from_wire(&wire).unwrap();
        if let EventPayload::Update { completed_on, .. } = back.payload {
            assert_eq!(completed_on, None);
        } else {
            panic!("expected update payload");
        }
    }

    #[test]
    fn wire_roundtrip_update_mark_undone() {
        // Some(None) = mark undone: must survive the wire roundtrip.
        let event = Event {
            event_type: EventType::Update,
            payload: EventPayload::Update {
                description: None,
                due_date: None,
                completed_on: Some(None),
            },
            ..sample_event()
        };
        let wire = to_wire(&event);
        let back = from_wire(&wire).unwrap();
        if let EventPayload::Update { completed_on, .. } = back.payload {
            assert_eq!(completed_on, Some(None));
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
        )
        .unwrap();
        assert_eq!(
            url,
            "https://relay.example.com/auth/login?cli_callback=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback%3Fstate%3Dabc"
        );
    }

    #[test]
    fn build_login_url_round_trips_callback() {
        let callback = "http://127.0.0.1:54321/callback?state=abc";
        let url = build_login_url("https://relay.example.com", callback).unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let cli_callback: String = parsed
            .query_pairs()
            .find(|(k, _)| k == "cli_callback")
            .unwrap()
            .1
            .into();
        assert_eq!(cli_callback, callback);
    }

    #[test]
    fn validate_relay_url_accepts_http_and_https_with_host() {
        assert_eq!(
            canonical_relay_url("https://relay.example.com").unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            canonical_relay_url("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            canonical_relay_url("  https://relay.example.com/  ").unwrap(),
            "https://relay.example.com"
        );
    }

    #[test]
    fn canonical_relay_url_preserves_ipv6_brackets() {
        assert_eq!(
            canonical_relay_url("http://[::1]:8080").unwrap(),
            "http://[::1]:8080"
        );
        assert_eq!(canonical_relay_url("http://[::1]/").unwrap(), "http://[::1]");
        assert_eq!(
            canonical_relay_url("http://[2001:db8::1]:3000/relay/").unwrap(),
            "http://[2001:db8::1]:3000/relay"
        );
        // The canonical form must be a usable request base for loopback relays.
        let canonical = canonical_relay_url("http://[::1]:8080/").unwrap();
        let parsed = reqwest::Url::parse(&format!("{canonical}/me")).unwrap();
        assert_eq!(parsed.host_str(), Some("[::1]"));
        assert_eq!(parsed.port(), Some(8080));
    }

    #[test]
    fn canonical_relay_url_normalizes_default_ports() {
        // An explicitly supplied scheme-default port must not make the same
        // relay look different from the spelling without the port.
        assert_eq!(
            canonical_relay_url("http://relay.example.com:80").unwrap(),
            canonical_relay_url("http://relay.example.com").unwrap()
        );
        assert_eq!(
            canonical_relay_url("https://relay.example.com:443/").unwrap(),
            canonical_relay_url("https://relay.example.com").unwrap()
        );
        // Non-default ports are significant and preserved.
        assert_eq!(
            canonical_relay_url("http://relay.example.com:8080").unwrap(),
            "http://relay.example.com:8080"
        );
        // A non-default port for the scheme stays distinct.
        assert_ne!(
            canonical_relay_url("https://relay.example.com:80").unwrap(),
            canonical_relay_url("https://relay.example.com").unwrap()
        );
    }

    #[test]
    fn validate_relay_url_rejects_bad_input() {
        for bad in [
            "",
            "   ",
            "relay.example.com",
            "ftp://relay.example.com",
            "file:///etc/passwd",
            "https://",
            "https://relay.example.com?tenant=1",
            "https://relay.example.com#fragment",
            "https://user@relay.example.com",
            "https://user:secret@relay.example.com",
        ] {
            assert!(
                canonical_relay_url(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn build_login_url_rejects_invalid_relay_url() {
        let result = build_login_url("not a url", "http://127.0.0.1:8080/callback?state=abc");
        assert!(matches!(result, Err(SyncError::InvalidRelayUrl)));
    }

    fn test_database() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn stored_credentials_alone_are_prior_sync_evidence() {
        let db = test_database();

        // No cursors, no stored credentials: eligible for first binding.
        let config = Config::default();
        assert!(!has_prior_sync_relationship(&db, &config).unwrap());

        // Cursor keys alone are evidence.
        let mut db = test_database();
        db.set_state("last_pulled_seq", "3").unwrap();
        assert!(has_prior_sync_relationship(&db, &config).unwrap());

        // Stored credentials alone are evidence too: an older CLI could log
        // in without ever completing a sync, leaving no cursors behind.
        let db = test_database();
        let config = Config {
            sync_url: Some("https://relay.example.com".to_string()),
            session_token: Some("token".to_string()),
            device_id: None,
        };
        assert!(has_prior_sync_relationship(&db, &config).unwrap());

        // A sync_url without a token (e.g. after logout) is not, on its own,
        // credentials — but logout preserves cursors, so the cursor check
        // still covers that case for datasets that actually synced.
        let config = Config {
            sync_url: Some("https://relay.example.com".to_string()),
            session_token: None,
            device_id: None,
        };
        assert!(!has_prior_sync_relationship(&db, &config).unwrap());
    }

    #[test]
    fn pull_cursor_stops_at_incompatible_event() {
        let mut db = test_database();
        let good = to_wire(&sample_event());
        let mut bad = to_wire(&sample_event());
        bad.event_type = "future_type".to_string();

        let pulled = vec![
            todo_common::wire::PulledEvent {
                seq: 1,
                event: good,
            },
            todo_common::wire::PulledEvent { seq: 2, event: bad },
            todo_common::wire::PulledEvent {
                seq: 3,
                event: to_wire(&sample_event()),
            },
        ];

        let result = apply_pulled_events(&mut db, 0, &pulled);
        assert!(result.is_err());

        // Only the first event was applied; the cursor stays before the bad one.
        assert_eq!(db.all_events().unwrap().len(), 1);
        assert_eq!(
            db.get_state("last_pulled_seq").unwrap().as_deref(),
            Some("1")
        );

        // Retrying from the saved cursor re-fetches the bad event first.
        let next = apply_pulled_events(&mut db, 1, &pulled[1..]);
        assert!(next.is_err());
        assert_eq!(
            db.get_state("last_pulled_seq").unwrap().as_deref(),
            Some("1")
        );
    }

    #[test]
    fn pull_cursor_advances_on_clean_pass() {
        let mut db = test_database();
        let events = vec![
            todo_common::wire::PulledEvent {
                seq: 1,
                event: to_wire(&sample_event()),
            },
            todo_common::wire::PulledEvent {
                seq: 2,
                event: to_wire(&sample_event()),
            },
        ];

        let count = apply_pulled_events(&mut db, 0, &events).unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.all_events().unwrap().len(), 2);
        assert_eq!(
            db.get_state("last_pulled_seq").unwrap().as_deref(),
            Some("2")
        );
    }
}
