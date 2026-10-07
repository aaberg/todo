use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A wire event — what the relay stores and forwards.
/// The relay treats `event_type` and `payload` as opaque strings;
/// it never deserializes them into domain types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    pub event_id: Uuid,
    pub device_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub todo_uuid: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// POST /push request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub events: Vec<WireEvent>,
}

/// POST /push response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub accepted: usize,
}

/// GET /pull query parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct PullQuery {
    /// Only return events with seq > this value.
    pub since: i64,
    /// Exclude events from this device.
    pub exclude_device: Uuid,
}

/// A pulled event — wire event plus the relay-assigned sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulledEvent {
    pub seq: i64,
    #[serde(flatten)]
    pub event: WireEvent,
}

/// GET /pull response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub events: Vec<PulledEvent>,
    pub latest_seq: i64,
}

/// GET /me response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeResponse {
    pub user_id: Uuid,
    pub email: Option<String>,
}

/// POST /auth/logout response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutResponse {
    pub ok: bool,
}

/// Standard error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
