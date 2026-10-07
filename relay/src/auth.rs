//! OIDC authentication flow and session middleware.
//!
//! Implemented in the auth phase. For now, this module provides
//! the session extraction middleware so api.rs compiles.

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::db::RelayDb;

/// Extracted from the `Authorization: Bearer <token>` header.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub user_id: Uuid,
}

/// Middleware: validate Bearer token, insert AuthSession into request extensions.
/// Returns 401 if missing or invalid.
pub async fn require_auth(
    State(db): State<std::sync::Arc<std::sync::Mutex<RelayDb>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let Some(token) = auth_header.strip_prefix("Bearer ") else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let user_id = {
        let db = match db.lock() {
            Ok(db) => db,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match db.validate_session(token) {
            Ok(Some(uid)) => uid,
            Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };

    request.extensions_mut().insert(AuthSession { user_id });
    next.run(request).await
}
