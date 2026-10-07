use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Create,
    Update,
    Delete,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::Create => "create",
            EventType::Update => "update",
            EventType::Delete => "delete",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "create" => Some(EventType::Create),
            "update" => Some(EventType::Update),
            "delete" => Some(EventType::Delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    Create {
        description: String,
        due_date: NaiveDate,
    },
    Update {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        due_date: Option<NaiveDate>,
        /// None = mark undone, Some(None) = no change, Some(Some(date)) = mark done
        #[serde(skip_serializing_if = "Option::is_none")]
        completed_on: Option<Option<NaiveDate>>,
    },
    Delete,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub event_id: Uuid,
    pub device_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub todo_uuid: Uuid,
    pub event_type: EventType,
    pub payload: EventPayload,
}

impl Event {
    pub fn new(
        device_id: Uuid,
        todo_uuid: Uuid,
        event_type: EventType,
        payload: EventPayload,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            device_id,
            timestamp: Utc::now(),
            todo_uuid,
            event_type,
            payload,
        }
    }
}
