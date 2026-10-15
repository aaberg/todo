use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Serde support for `Option<Option<T>>`:
/// absent field = outer None, JSON null = Some(None), value = Some(Some(T)).
/// Without this, serde cannot tell `Some(None)` apart from `None` when both
/// round-trip through JSON, which silently turned "mark undone" updates into
/// "no change" after a DB roundtrip.
pub mod double_option {
    use super::*;

    pub fn serialize<S>(value: &Option<Option<NaiveDate>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(inner) => serializer.serialize_some(inner),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<NaiveDate>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let inner = Option::<NaiveDate>::deserialize(deserializer)?;
        Ok(Some(inner))
    }
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "double_option::serialize",
            deserialize_with = "double_option::deserialize"
        )]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_completed_on_roundtrips_through_json() {
        // Some(Some(date)) = mark done
        let done = EventPayload::Update {
            description: None,
            due_date: None,
            completed_on: Some(Some(NaiveDate::from_ymd_opt(2026, 10, 15).unwrap())),
        };
        let json = serde_json::to_string(&done).unwrap();
        assert_eq!(
            serde_json::from_str::<EventPayload>(&json).unwrap(),
            done
        );

        // Some(None) = mark undone: must survive a JSON roundtrip as a
        // "clear completed_on" update, not collapse to "no change".
        let undone = EventPayload::Update {
            description: None,
            due_date: None,
            completed_on: Some(None),
        };
        let json = serde_json::to_string(&undone).unwrap();
        assert_eq!(
            serde_json::from_str::<EventPayload>(&json).unwrap(),
            undone
        );

        // None = field not set = no change to completion state.
        let no_change = EventPayload::Update {
            description: None,
            due_date: None,
            completed_on: None,
        };
        let json = serde_json::to_string(&no_change).unwrap();
        assert_eq!(
            serde_json::from_str::<EventPayload>(&json).unwrap(),
            no_change
        );
    }
}
