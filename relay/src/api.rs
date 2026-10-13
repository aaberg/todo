//! HTTP API endpoints.

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use todo_common::wire::*;

use crate::auth::{
    admit_pending, AppState, AuthSession, PendingLogin, MAX_CALLBACK_BYTES, PENDING_LOGIN_LIFETIME,
};

/// GET /health — no auth required.
pub async fn health() -> &'static str {
    "ok"
}

/// GET / — no auth required.
///
/// Human-facing status page. Confirms the relay is up and reachable at this URL.
pub async fn status_page(State(state): State<AppState>) -> Html<String> {
    let public_url = html_escape(state.config.public_url.as_str());
    Html(STATUS_PAGE_HTML.replace("{{PUBLIC_URL}}", &public_url))
}

/// Escape text for safe interpolation into HTML content.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STATUS_PAGE_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>todo relay — status</title>
<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; margin: 0; }
  body {
    font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    background: #f2f4f8;
    color: #1c2733;
    padding: 1.5rem;
  }
  @media (prefers-color-scheme: dark) {
    body { background: #10161d; color: #e4e9ee; }
    .card { background: #1a232c !important; box-shadow: 0 20px 50px rgba(0,0,0,.5) !important; }
    .url { background: #10161d !important; color: #9fb3c8 !important; }
    .status-badge { background: rgba(52, 199, 123, .15) !important; color: #34c77b !important; }
  }
  .card {
    background: #ffffff;
    border-radius: 16px;
    box-shadow: 0 20px 50px rgba(28, 39, 51, .12);
    padding: 2.5rem 3rem;
    max-width: 30rem;
    width: 100%;
    text-align: center;
  }
  .dot {
    width: 14px; height: 14px;
    border-radius: 50%;
    background: #34c77b;
    display: inline-block;
    margin-right: 0.6rem;
    box-shadow: 0 0 0 4px rgba(52, 199, 123, .25);
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { box-shadow: 0 0 0 4px rgba(52, 199, 123, .25); }
    50% { box-shadow: 0 0 0 8px rgba(52, 199, 123, .1); }
  }
  h1 { font-size: 1.25rem; font-weight: 600; letter-spacing: -0.01em; }
  .status-badge {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    margin-top: 1.25rem;
    padding: 0.4rem 1rem;
    border-radius: 999px;
    background: rgba(52, 199, 123, .12);
    color: #1e9e5c;
    font-size: 0.85rem;
    font-weight: 600;
  }
  .url {
    margin-top: 1.5rem;
    background: #f2f4f8;
    border-radius: 8px;
    padding: 0.6rem 1rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.85rem;
    color: #51616f;
    overflow-wrap: anywhere;
  }
  footer { margin-top: 1.5rem; font-size: 0.8rem; color: #8a98a5; }
</style>
</head>
<body>
  <main class="card">
    <h1><span class="dot"></span>todo relay</h1>
    <span class="status-badge">operational</span>
    <div class="url">{{PUBLIC_URL}}</div>
    <footer>Sync relay for the local-first todo CLI. Serving — page reached, relay is up.</footer>
  </main>
</body>
</html>"##;

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

    // Bound the variable-sized data retained per flow, before any pending-state allocation.
    if cli_callback.len() > MAX_CALLBACK_BYTES {
        return (StatusCode::BAD_REQUEST, "cli_callback is too long").into_response();
    }

    // Validate the callback URL is a loopback address (security: prevent open redirect)
    if !is_valid_loopback_url(&cli_callback) {
        return (StatusCode::BAD_REQUEST, "cli_callback must be a loopback URL").into_response();
    }

    let (authorize_url, csrf_token, nonce) = state.oidc.authorize_url();

    // Admit the pending login under one critical section: prune expired
    // entries, check capacity, then insert. Never evict a live flow.
    {
        let mut pending = match state.pending_logins.lock() {
            Ok(p) => p,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let now = std::time::Instant::now();
        let entry = PendingLogin {
            nonce,
            cli_callback,
            expires_at: now + PENDING_LOGIN_LIFETIME,
        };
        if !admit_pending(&mut pending, csrf_token.secret().clone(), entry, now) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "login capacity temporarily exhausted, retry later",
            )
                .into_response();
        }
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
            Some(p) => {
                if p.is_expired(std::time::Instant::now()) {
                    return (
                        StatusCode::BAD_REQUEST,
                        "invalid or expired state",
                    )
                        .into_response();
                }
                p
            }
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

    // Redirect to CLI's loopback with the session token, preserving any
    // existing callback query (e.g. the CLI's per-attempt state)
    let mut redirect = url::Url::parse(&pending.cli_callback)
        .expect("stored cli_callback was validated as a URL");
    redirect
        .query_pairs_mut()
        .append_pair("token", &token);
    Redirect::to(redirect.as_str()).into_response()
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
