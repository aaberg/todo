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

/// How long a pending login flow stays valid after admission.
/// Bounds flow validity; aligned with the CLI's five-minute login timeout.
pub const PENDING_LOGIN_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Maximum number of live pending login flows retained at once.
pub const MAX_PENDING_LOGINS: usize = 1024;

/// Maximum decoded byte length accepted for a `cli_callback` URL.
pub const MAX_CALLBACK_BYTES: usize = 2048;

/// Shared application state, cloned into every request.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<RelayDb>>,
    pub oidc: Arc<OidcClient>,
    pub config: Arc<crate::config::Config>,
    /// Pending login flows: state → (csrf_token, nonce, cli_callback_url)
    pub pending_logins: Arc<Mutex<HashMap<String, PendingLogin>>>,
}

/// A login flow in progress, waiting for the OIDC callback.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub nonce: Nonce,
    /// Where to send the session token after successful login
    /// (the CLI's loopback URL, e.g. http://127.0.0.1:54321/callback)
    pub cli_callback: String,
    /// Monotonic deadline after which this flow is no longer usable.
    pub expires_at: Instant,
}

impl PendingLogin {
    /// A flow is expired when its deadline has arrived or passed.
    pub fn is_expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }
}

/// Drop expired entries from the pending-login map.
pub fn prune_expired(pending: &mut HashMap<String, PendingLogin>, now: Instant) {
    pending.retain(|_, entry| !entry.is_expired(now));
}

/// Prune expired entries, then admit a new flow if live capacity remains.
/// Returns `false` (without inserting or evicting) when the map is at capacity.
pub fn admit_pending(
    pending: &mut HashMap<String, PendingLogin>,
    key: String,
    entry: PendingLogin,
    now: Instant,
) -> bool {
    prune_expired(pending, now);
    if pending.len() >= MAX_PENDING_LOGINS {
        return false;
    }
    pending.insert(key, entry);
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
    use openidconnect::Nonce;

    fn entry(expires_at: Instant) -> PendingLogin {
        PendingLogin {
            nonce: Nonce::new("test-nonce".to_string()),
            cli_callback: "http://127.0.0.1:54321/callback".to_string(),
            expires_at,
        }
    }

    #[test]
    fn expiry_boundary() {
        let now = Instant::now();
        let live = entry(now + PENDING_LOGIN_LIFETIME);
        assert!(!live.is_expired(now));
        assert!(!live.is_expired(now + PENDING_LOGIN_LIFETIME - Duration::from_millis(1)));
        assert!(live.is_expired(now + PENDING_LOGIN_LIFETIME));
        assert!(live.is_expired(now + PENDING_LOGIN_LIFETIME + Duration::from_secs(1)));
    }

    #[test]
    fn prune_expired_drops_only_expired_entries() {
        let now = Instant::now();
        let mut pending = HashMap::new();
        pending.insert("expired".to_string(), entry(now - Duration::from_secs(1)));
        pending.insert("live".to_string(), entry(now + Duration::from_secs(60)));
        prune_expired(&mut pending, now);
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key("live"));
    }

    #[test]
    fn admit_rejects_at_capacity_without_evicting() {
        let now = Instant::now();
        let mut pending = HashMap::new();
        for i in 0..MAX_PENDING_LOGINS {
            pending.insert(format!("s{i}"), entry(now + Duration::from_secs(60)));
        }
        assert!(!admit_pending(
            &mut pending,
            "new".to_string(),
            entry(now + PENDING_LOGIN_LIFETIME),
            now
        ));
        assert_eq!(pending.len(), MAX_PENDING_LOGINS);
        assert!(!pending.contains_key("new"));
    }

    #[test]
    fn admit_reclaims_expired_slots_before_capacity_check() {
        let now = Instant::now();
        let mut pending = HashMap::new();
        for i in 0..MAX_PENDING_LOGINS {
            let deadline = if i == 0 {
                now - Duration::from_secs(1)
            } else {
                now + Duration::from_secs(60)
            };
            pending.insert(format!("s{i}"), entry(deadline));
        }
        assert!(admit_pending(
            &mut pending,
            "new".to_string(),
            entry(now + PENDING_LOGIN_LIFETIME),
            now
        ));
        assert_eq!(pending.len(), MAX_PENDING_LOGINS);
        assert!(pending.contains_key("new"));
        assert!(!pending.contains_key("s0"));
    }

    #[test]
    fn admit_never_exceeds_cap() {
        let base = Instant::now();
        let mut pending = HashMap::new();
        for i in 0..(MAX_PENDING_LOGINS + 10) {
            let now = base + Duration::from_millis(i as u64);
            admit_pending(
                &mut pending,
                format!("s{i}"),
                entry(now + PENDING_LOGIN_LIFETIME),
                now,
            );
        }
        assert!(pending.len() <= MAX_PENDING_LOGINS);
    }
}
