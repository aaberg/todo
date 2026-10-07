//! Authentication: session middleware and login state management.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use openidconnect::{CsrfToken, Nonce};
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

/// A login flow in progress, waiting for the OIDC callback.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub csrf_token: CsrfToken,
    pub nonce: Nonce,
    /// Where to send the session token after successful login
    /// (the CLI's loopback URL, e.g. http://127.0.0.1:54321/callback)
    pub cli_callback: String,
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
