//! HTTP API endpoints.

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    Json,
};
use std::sync::{Arc, Mutex};
use todo_common::wire::*;
use uuid::Uuid;

use crate::auth::AuthSession;
use crate::db::RelayDb;

type SharedDb = Arc<Mutex<RelayDb>>;

pub async fn push(
    State(db): State<SharedDb>,
    Extension(session): Extension<AuthSession>,
    Json(request): Json<PushRequest>,
) -> Result<Json<PushResponse>, StatusCode> {
    let mut db = db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let accepted = db
        .insert_events(session.user_id, &request.events)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PushResponse { accepted }))
}

pub async fn pull(
    State(db): State<SharedDb>,
    Extension(session): Extension<AuthSession>,
    Query(query): Query<PullQuery>,
) -> Result<Json<PullResponse>, StatusCode> {
    let db = db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let events = db
        .pull_events(session.user_id, query.exclude_device, query.since, 500)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let latest_seq = db
        .latest_seq(session.user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PullResponse { events, latest_seq }))
}

pub async fn me(
    State(db): State<SharedDb>,
    Extension(session): Extension<AuthSession>,
) -> Result<Json<MeResponse>, StatusCode> {
    let db = db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let email = db
        .user_email(session.user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(MeResponse {
        user_id: session.user_id,
        email,
    }))
}

pub async fn logout(
    State(db): State<SharedDb>,
    headers: axum::http::HeaderMap,
) -> Result<Json<LogoutResponse>, StatusCode> {
    let db = db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

/// Health check — no auth required.
pub async fn health() -> &'static str {
    "ok"
}

/// Placeholder for OIDC login — implemented in the auth phase.
pub async fn auth_login() -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "OIDC login not yet implemented")
}

/// Placeholder for OIDC callback — implemented in the auth phase.
pub async fn auth_callback() -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "OIDC callback not yet implemented")
}

/// Helper: get the Bearer token from headers (used by logout).
#[allow(dead_code)]
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Helper: get user_id from AuthSession extension.
#[allow(dead_code)]
pub fn user_id(session: &AuthSession) -> Uuid {
    session.user_id
}
