//! Authentication: session middleware and login state management.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use openidconnect::Nonce;
use uuid::Uuid;

use crate::{db::RelayDb, oidc::OidcClient};

/// Shared application state, cloned into every request.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<RelayDb>>,
    pub oidc: Arc<OidcClient>,
    pub config: Arc<crate::config::Config>,
    /// Pending login flows: state → (csrf_token, nonce, cli_callback_url)
    pub pending_logins: Arc<Mutex<HashMap<String, PendingLogin>>>,
}

/// How long a pending login flow stays valid after admission. Matches the
/// CLI's five-minute login timeout.
pub const PENDING_LOGIN_TTL: Duration = Duration::from_secs(5 * 60);

/// Maximum number of concurrently live pending login flows. New logins are
/// rejected with 503 once this many unexpired entries exist.
pub const MAX_PENDING_LOGINS: usize = 1_024;

/// Maximum decoded byte length of a `cli_callback` URL.
pub const MAX_CALLBACK_URL_BYTES: usize = 2_048;

/// A login flow in progress, waiting for the OIDC callback.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub nonce: Nonce,
    /// Where to send the session token after successful login
    /// (the CLI's loopback URL, e.g. http://127.0.0.1:54321/callback)
    pub cli_callback: String,
    /// Monotonic deadline: the flow is expired once `expires_at <= now`.
    pub expires_at: Instant,
}

impl PendingLogin {
    pub fn new(nonce: Nonce, cli_callback: String, now: Instant) -> Self {
        Self {
            nonce,
            cli_callback,
            expires_at: now + PENDING_LOGIN_TTL,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }
}

/// Remove expired entries from the pending-login map.
pub fn prune_expired(map: &mut HashMap<String, PendingLogin>, now: Instant) {
    map.retain(|_, login| !login.is_expired(now));
}

/// Admit a new pending login: prune expired entries, reject if the map is
/// still at capacity, otherwise insert. Must be called with the map's mutex
/// held so concurrent requests cannot exceed the cap.
pub fn admit_pending(
    map: &mut HashMap<String, PendingLogin>,
    state: String,
    login: PendingLogin,
    now: Instant,
) -> bool {
    prune_expired(map, now);
    if map.len() >= MAX_PENDING_LOGINS {
        return false;
    }
    map.insert(state, login);
    true
}

/// Extracted from the `Authorization: Bearer <token>` header.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub user_id: Uuid,
}

/// Middleware: validate Bearer token, insert AuthSession into request extensions.
/// Returns 401 if missing or invalid.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let Some(token) = auth_header.strip_prefix("Bearer ") else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let user_id = {
        let db = match state.db.lock() {
            Ok(db) => db,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match db.validate_session(token) {
            Ok(Some(uid)) => uid,
            Ok(None) => {
                return (StatusCode::UNAUTHORIZED, "invalid or expired session").into_response();
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };

    request.extensions_mut().insert(AuthSession { user_id });
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(expires_at: Instant) -> PendingLogin {
        PendingLogin {
            nonce: Nonce::new("test-nonce".to_string()),
            cli_callback: "http://127.0.0.1:54321/callback".to_string(),
            expires_at,
        }
    }

    fn full_map(now: Instant) -> HashMap<String, PendingLogin> {
        let mut map = HashMap::new();
        for i in 0..MAX_PENDING_LOGINS {
            map.insert(format!("state-{i}"), entry(now + PENDING_LOGIN_TTL));
        }
        map
    }

    fn full_map_with_one_expired(now: Instant) -> HashMap<String, PendingLogin> {
        let mut map = HashMap::new();
        for i in 0..MAX_PENDING_LOGINS - 1 {
            map.insert(format!("state-{i}"), entry(now + PENDING_LOGIN_TTL));
        }
        map.insert("expired".to_string(), entry(now - Duration::from_secs(1)));
        map
    }

    #[test]
    fn new_sets_deadline_from_admission_time() {
        let now = Instant::now();
        let login = PendingLogin::new(
            Nonce::new("n".to_string()),
            "http://127.0.0.1:1/cb".to_string(),
            now,
        );
        assert_eq!(login.expires_at, now + PENDING_LOGIN_TTL);
        assert!(!login.is_expired(now));
    }

    #[test]
    fn flow_just_before_deadline_is_live_at_or_after_is_expired() {
        let now = Instant::now();
        let login = entry(now + PENDING_LOGIN_TTL);
        assert!(!login.is_expired(login.expires_at - Duration::from_nanos(1)));
        assert!(login.is_expired(login.expires_at));
        assert!(login.is_expired(login.expires_at + Duration::from_secs(1)));
    }

    #[test]
    fn prune_expired_removes_only_expired_entries() {
        let now = Instant::now();
        let mut map = HashMap::new();
        map.insert("live".to_string(), entry(now + Duration::from_secs(60)));
        map.insert("dead".to_string(), entry(now - Duration::from_secs(1)));
        prune_expired(&mut map, now);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("live"));
    }

    #[test]
    fn admit_rejects_when_full_of_live_entries() {
        let now = Instant::now();
        let mut map = full_map(now);
        let before = map.len();
        let admitted = admit_pending(
            &mut map,
            "new-state".to_string(),
            entry(now + PENDING_LOGIN_TTL),
            now,
        );
        assert!(!admitted);
        assert_eq!(map.len(), before);
        assert!(!map.contains_key("new-state"));
    }

    #[test]
    fn admit_reclaims_expired_before_capacity_check() {
        let now = Instant::now();
        let mut map = full_map_with_one_expired(now);
        let admitted = admit_pending(
            &mut map,
            "new-state".to_string(),
            entry(now + PENDING_LOGIN_TTL),
            now,
        );
        assert!(admitted);
        assert_eq!(map.len(), MAX_PENDING_LOGINS);
        assert!(map.contains_key("new-state"));
        assert!(!map.contains_key("expired"));
    }

    #[test]
    fn admit_accepts_within_capacity_and_prunes() {
        let now = Instant::now();
        let mut map = HashMap::new();
        map.insert("expired".to_string(), entry(now - Duration::from_secs(1)));
        let admitted = admit_pending(
            &mut map,
            "new-state".to_string(),
            entry(now + PENDING_LOGIN_TTL),
            now,
        );
        assert!(admitted);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("new-state"));
    }

    #[test]
    fn max_pending_logins_capacity_is_enforced() {
        let now = Instant::now();
        let mut map = HashMap::new();
        for i in 0..MAX_PENDING_LOGINS {
            let admitted = admit_pending(
                &mut map,
                format!("state-{i}"),
                entry(now + PENDING_LOGIN_TTL),
                now,
            );
            assert!(admitted);
        }
        assert_eq!(map.len(), MAX_PENDING_LOGINS);
        assert!(!admit_pending(
            &mut map,
            "overflow".to_string(),
            entry(now + PENDING_LOGIN_TTL),
            now
        ));
        assert_eq!(map.len(), MAX_PENDING_LOGINS);
    }

    #[test]
    fn callback_size_limit_bounds_are_exact() {
        assert_eq!(MAX_CALLBACK_URL_BYTES, 2_048);
        let exact: String = "x".repeat(MAX_CALLBACK_URL_BYTES);
        assert!(exact.len() <= MAX_CALLBACK_URL_BYTES);
        let over: String = "x".repeat(MAX_CALLBACK_URL_BYTES + 1);
        assert!(over.len() > MAX_CALLBACK_URL_BYTES);
    }
}
