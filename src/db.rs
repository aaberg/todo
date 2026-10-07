use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::event::{Event, EventPayload, EventType};

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode = WAL;")?;
        let mut database = Self { connection };
        database.initialize()?;
        Ok(database)
    }

    fn initialize(&mut self) -> rusqlite::Result<()> {
        // Clean slate: drop legacy todos table if it exists (Phase 1 artifact)
        self.connection.execute_batch("DROP TABLE IF EXISTS todos;")?;

        self.connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS events (
                event_id TEXT PRIMARY KEY,
                device_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                todo_uuid TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_todo ON events(todo_uuid);
            CREATE INDEX IF NOT EXISTS idx_events_order ON events(timestamp, event_id);

            CREATE TABLE IF NOT EXISTS sync_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )?;

        // Ensure device_id exists
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sync_state WHERE key = 'device_id'",
            [],
            |row| row.get(0),
        )?;
        if count == 0 {
            self.connection.execute(
                "INSERT INTO sync_state (key, value) VALUES ('device_id', ?1)",
                params![Uuid::new_v4().to_string()],
            )?;
        }

        Ok(())
    }

    pub fn device_id(&self) -> rusqlite::Result<Uuid> {
        let value: String = self.connection.query_row(
            "SELECT value FROM sync_state WHERE key = 'device_id'",
            [],
            |row| row.get(0),
        )?;
        Uuid::parse_str(&value).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    }

    /// Append an event to the local log. This is the ONLY write path.
    pub fn append_event(&mut self, event: &Event) -> rusqlite::Result<()> {
        let payload = serde_json::to_string(&event.payload).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;
        self.connection.execute(
            "INSERT OR IGNORE INTO events (event_id, device_id, timestamp, todo_uuid, event_type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.event_id.to_string(),
                event.device_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.todo_uuid.to_string(),
                event.event_type.as_str(),
                payload,
            ],
        )?;
        Ok(())
    }

    /// Fetch all events ordered by (timestamp, event_id) for deterministic folding.
    pub fn all_events(&self) -> rusqlite::Result<Vec<Event>> {
        let mut stmt = self.connection.prepare(
            "SELECT event_id, device_id, timestamp, todo_uuid, event_type, payload
             FROM events
             ORDER BY timestamp, event_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut events = Vec::new();
        for row in rows {
            let (event_id, device_id, timestamp, todo_uuid, event_type, payload) = row?;
            events.push(Event {
                event_id: parse_uuid(&event_id)?,
                device_id: parse_uuid(&device_id)?,
                timestamp: parse_datetime(&timestamp)?,
                todo_uuid: parse_uuid(&todo_uuid)?,
                event_type: EventType::from_str(&event_type)
                    .ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(
                        4, rusqlite::types::Type::Text,
                        format!("unknown event_type: {event_type}").into(),
                    ))?,
                payload: serde_json::from_str(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5, rusqlite::types::Type::Text, Box::new(e),
                    )
                })?,
            });
        }
        Ok(events)
    }

    /// Fetch events for a specific todo (for `todo log`).
    pub fn events_for_todo(&self, todo_uuid: Uuid) -> rusqlite::Result<Vec<Event>> {
        let mut stmt = self.connection.prepare(
            "SELECT event_id, device_id, timestamp, todo_uuid, event_type, payload
             FROM events
             WHERE todo_uuid = ?1
             ORDER BY timestamp, event_id",
        )?;
        let rows = stmt.query_map(params![todo_uuid.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut events = Vec::new();
        for row in rows {
            let (event_id, device_id, timestamp, todo_uuid, event_type, payload) = row?;
            events.push(Event {
                event_id: parse_uuid(&event_id)?,
                device_id: parse_uuid(&device_id)?,
                timestamp: parse_datetime(&timestamp)?,
                todo_uuid: parse_uuid(&todo_uuid)?,
                event_type: EventType::from_str(&event_type)
                    .ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(
                        4, rusqlite::types::Type::Text,
                        format!("unknown event_type: {event_type}").into(),
                    ))?,
                payload: serde_json::from_str(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5, rusqlite::types::Type::Text, Box::new(e),
                    )
                })?,
            });
        }
        Ok(events)
    }

    /// Get a sync_state value.
    pub fn get_state(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let result = self.connection.query_row(
            "SELECT value FROM sync_state WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Set a sync_state value.
    pub fn set_state(&mut self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO sync_state (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Convenience: append an event in a single call.
    pub fn emit(
        &mut self,
        todo_uuid: Uuid,
        event_type: EventType,
        payload: EventPayload,
    ) -> rusqlite::Result<Event> {
        let device_id = self.device_id()?;
        let event = Event::new(device_id, todo_uuid, event_type, payload);
        self.append_event(&event)?;
        Ok(event)
    }
}

fn parse_uuid(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_datetime(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use crate::model::fold_events;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn test_database() -> Database {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        let mut database = Database { connection };
        database.initialize().unwrap();
        database
    }

    #[test]
    fn device_id_is_stable() {
        let db = test_database();
        let a = db.device_id().unwrap();
        let b = db.device_id().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn append_and_read_event_roundtrip() {
        let mut db = test_database();
        let todo_uuid = Uuid::new_v4();
        db.emit(
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Test".to_owned(),
                due_date: date("2026-10-08"),
            },
        )
        .unwrap();

        let events = db.all_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].todo_uuid, todo_uuid);
        assert_eq!(events[0].event_type, EventType::Create);
    }

    #[test]
    fn duplicate_event_id_is_ignored() {
        let mut db = test_database();
        let device = db.device_id().unwrap();
        let todo_uuid = Uuid::new_v4();
        let event = Event::new(
            device,
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Test".to_owned(),
                due_date: date("2026-10-08"),
            },
        );
        db.append_event(&event).unwrap();
        db.append_event(&event).unwrap(); // duplicate — should be ignored

        let events = db.all_events().unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn events_are_ordered_by_timestamp() {
        let mut db = test_database();
        let device = db.device_id().unwrap();
        let todo_a = Uuid::new_v4();
        let todo_b = Uuid::new_v4();

        // Insert out of order (b first, then a with earlier timestamp)
        let mut event_b = Event::new(
            device,
            todo_b,
            EventType::Create,
            EventPayload::Create {
                description: "B".to_owned(),
                due_date: date("2026-10-08"),
            },
        );
        event_b.timestamp = DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        db.append_event(&event_b).unwrap();

        let mut event_a = Event::new(
            device,
            todo_a,
            EventType::Create,
            EventPayload::Create {
                description: "A".to_owned(),
                due_date: date("2026-10-08"),
            },
        );
        event_a.timestamp = DateTime::parse_from_rfc3339("2026-10-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        db.append_event(&event_a).unwrap();

        let events = db.all_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].todo_uuid, todo_a); // earlier timestamp first
        assert_eq!(events[1].todo_uuid, todo_b);
    }

    #[test]
    fn events_for_todo_filters_correctly() {
        let mut db = test_database();
        let todo_a = Uuid::new_v4();
        let todo_b = Uuid::new_v4();

        db.emit(
            todo_a,
            EventType::Create,
            EventPayload::Create {
                description: "A".to_owned(),
                due_date: date("2026-10-08"),
            },
        )
        .unwrap();
        db.emit(
            todo_b,
            EventType::Create,
            EventPayload::Create {
                description: "B".to_owned(),
                due_date: date("2026-10-08"),
            },
        )
        .unwrap();

        let events = db.events_for_todo(todo_a).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].todo_uuid, todo_a);
    }

    #[test]
    fn fold_after_emit_produces_todo() {
        let mut db = test_database();
        let todo_uuid = Uuid::new_v4();
        db.emit(
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Test todo".to_owned(),
                due_date: date("2026-10-08"),
            },
        )
        .unwrap();

        let events = db.all_events().unwrap();
        let todos = fold_events(&events);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].description, "Test todo");
    }

    #[test]
    fn legacy_todos_table_is_dropped() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        // Simulate a legacy database with the old todos table
        connection.execute_batch(
            "CREATE TABLE todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                description TEXT NOT NULL,
                due_date TEXT NOT NULL,
                completed_on TEXT
            );
            INSERT INTO todos (description, due_date) VALUES ('old', '2026-10-08');
            ",
        )
        .unwrap();

        let mut db = Database { connection };
        db.initialize().unwrap();

        // todos table should be gone
        let result = db.connection.execute_batch("SELECT * FROM todos;");
        assert!(result.is_err());

        // events table should exist
        let count: i64 = db
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn sync_state_roundtrip() {
        let mut db = test_database();
        assert!(db.get_state("missing").unwrap().is_none());
        db.set_state("key", "value").unwrap();
        assert_eq!(db.get_state("key").unwrap(), Some("value".to_owned()));
        db.set_state("key", "updated").unwrap();
        assert_eq!(db.get_state("key").unwrap(), Some("updated".to_owned()));
    }
}
