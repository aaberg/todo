//! HTTP API endpoints.

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use todo_common::wire::*;

use crate::auth::{AppState, AuthSession};

/// GET /health — no auth required.
pub async fn health() -> &'static str {
    "ok"
}

/// GET /auth/login?cli_callback=<url>
///
/// Starts the OIDC login flow. The `cli_callback` parameter is the CLI's
/// loopback URL where the session token will eventually be delivered.
///
/// Returns a 302 redirect to the OIDC provider's authorization endpoint.
pub async fn auth_login(
    State(state): State<AppState>,
    Query(params): Query<LoginQuery>,
) -> Response {
    let cli_callback = match params.cli_callback {
        Some(url) => url,
        None => {
            return (StatusCode::BAD_REQUEST, "missing cli_callback parameter").into_response();
        }
    };

    // Validate the callback URL is a loopback address (security: prevent open redirect)
    if !is_valid_loopback_url(&cli_callback) {
        return (StatusCode::BAD_REQUEST, "cli_callback must be a loopback URL").into_response();
    }

    let (authorize_url, csrf_token, nonce) = state.oidc.authorize_url();

    // Store the pending login
    {
        let mut pending = match state.pending_logins.lock() {
            Ok(p) => p,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        pending.insert(
            csrf_token.secret().clone(),
            crate::auth::PendingLogin {
                csrf_token,
                nonce,
                cli_callback,
            },
        );
    }

    Redirect::to(authorize_url.as_str()).into_response()
}

/// GET /auth/callback?code=...&state=...
///
/// OIDC provider redirects here after the user authenticates.
/// Exchanges the code for tokens, validates the ID token, creates a session,
/// and redirects the browser to the CLI's loopback URL with the session token.
pub async fn auth_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
) -> Response {
    // Validate state and retrieve pending login
    let pending = {
        let mut pending_map = match state.pending_logins.lock() {
            Ok(p) => p,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match pending_map.remove(&params.state) {
            Some(p) => p,
            None => {
                return (StatusCode::BAD_REQUEST, "invalid or expired state").into_response();
            }
        }
    };

    // Exchange code for tokens and validate
    let identity = match state
        .oidc
        .exchange_code(&params.code, &pending.nonce, &state.config.allowed_groups)
        .await
    {
        Ok(identity) => identity,
        Err(crate::oidc::OidcError::NotInAllowedGroup) => {
            return (
                StatusCode::FORBIDDEN,
                Html(r#"<!DOCTYPE html><html><body><h1>Access denied</h1><p>You are not a member of an allowed group.</p></body></html>"#),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "OIDC code exchange failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication failed",
            )
                .into_response();
        }
    };

    // Find or create user
    let user_id = {
        let db = match state.db.lock() {
            Ok(db) => db,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match db.find_or_create_user(&identity.oidc_sub, identity.email.as_deref()) {
            Ok(uid) => uid,
            Err(e) => {
                tracing::error!(error = %e, "failed to create user");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    // Create session
    let (token, _token_hash) = {
        let mut db = match state.db.lock() {
            Ok(db) => db,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match db.create_session(user_id, state.config.session_ttl_secs) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "failed to create session");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    tracing::info!(user_id = %user_id, email = ?identity.email, "user logged in");

    // Redirect to CLI's loopback with the session token
    let redirect_url = format!("{}?token={}", pending.cli_callback, token);
    Redirect::to(&redirect_url).into_response()
}

/// POST /push — requires auth. Push local events to the relay.
pub async fn push(
    State(state): State<AppState>,
    Extension(session): Extension<AuthSession>,
    Json(request): Json<PushRequest>,
) -> Result<Json<PushResponse>, StatusCode> {
    let mut db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let accepted = db
        .insert_events(session.user_id, &request.events)
        .map_err(|e| {
            tracing::error!(error = %e, "failed to insert events");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(PushResponse { accepted }))
}

/// GET /pull?since=<seq>&exclude_device=<uuid> — requires auth.
pub async fn pull(
    State(state): State<AppState>,
    Extension(session): Extension<AuthSession>,
    Query(query): Query<PullQuery>,
) -> Result<Json<PullResponse>, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let events = db
        .pull_events(session.user_id, query.exclude_device, query.since, 500)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let latest_seq = db
        .latest_seq(session.user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PullResponse { events, latest_seq }))
}

/// GET /me — requires auth. Return current user info.
pub async fn me(
    State(state): State<AppState>,
    Extension(session): Extension<AuthSession>,
) -> Result<Json<MeResponse>, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let email = db
        .user_email(session.user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(MeResponse {
        user_id: session.user_id,
        email,
    }))
}

/// POST /auth/logout — revoke the session token.
pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<LogoutResponse>, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        db.delete_session(token)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(Json(LogoutResponse { ok: true }))
}

/// Query params for GET /auth/login.
#[derive(Debug, serde::Deserialize)]
pub struct LoginQuery {
    /// The CLI's loopback URL to redirect to after login.
    pub cli_callback: Option<String>,
}

/// Query params for GET /auth/callback.
#[derive(Debug, serde::Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

/// Validate that a URL is a loopback address (127.0.0.1, ::1, or localhost).
/// Prevents open redirect attacks via the cli_callback parameter.
fn is_valid_loopback_url(url_str: &str) -> bool {
    let Ok(url) = url::Url::parse(url_str) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    match url.host_str() {
        Some("127.0.0.1") | Some("[::1]") | Some("localhost") => true,
        _ => false,
    }
}
