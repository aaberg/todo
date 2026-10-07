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

    // ─── Convergence tests: two-machine sync simulation ───

    fn make_event(
        device: Uuid,
        todo: Uuid,
        event_type: EventType,
        payload: EventPayload,
        ts: &str,
    ) -> Event {
        let mut event = Event::new(device, todo, event_type, payload);
        event.timestamp = DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&Utc);
        event
    }

    /// Simulate a sync: push all of B's events to A and vice versa.
    /// Dedup is handled by INSERT OR IGNORE on event_id.
    fn exchange_events(a: &mut Database, b: &mut Database) {
        let a_events = a.all_events().unwrap();
        let b_events = b.all_events().unwrap();
        for event in &b_events {
            a.append_event(event).unwrap();
        }
        for event in &a_events {
            b.append_event(event).unwrap();
        }
    }

    fn folded_state(db: &Database) -> Vec<crate::model::Todo> {
        let events = db.all_events().unwrap();
        fold_events(&events)
    }

    #[test]
    fn convergence_independent_creates() {
        let mut a = test_database();
        let mut b = test_database();

        // Machine A creates a todo
        let todo_x = Uuid::new_v4();
        a.append_event(&make_event(
            a.device_id().unwrap(),
            todo_x,
            EventType::Create,
            EventPayload::Create {
                description: "From A".to_owned(),
                due_date: date("2026-10-08"),
            },
            "2026-10-07T10:00:00Z",
        ))
        .unwrap();

        // Machine B creates a different todo
        let todo_y = Uuid::new_v4();
        b.append_event(&make_event(
            b.device_id().unwrap(),
            todo_y,
            EventType::Create,
            EventPayload::Create {
                description: "From B".to_owned(),
                due_date: date("2026-10-09"),
            },
            "2026-10-07T11:00:00Z",
        ))
        .unwrap();

        exchange_events(&mut a, &mut b);

        let state_a = folded_state(&a);
        let state_b = folded_state(&b);
        assert_eq!(state_a, state_b);
        assert_eq!(state_a.len(), 2);
    }

    #[test]
    fn convergence_conflicting_edits_lww_by_timestamp() {
        let mut a = test_database();
        let mut b = test_database();
        let todo_uuid = Uuid::new_v4();

        // Both machines create the same todo (same UUID — as if synced once)
        let create = make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Original".to_owned(),
                due_date: date("2026-10-08"),
            },
            "2026-10-07T09:00:00Z",
        );
        a.append_event(&create).unwrap();
        b.append_event(&create).unwrap();

        // Both machines edit the description concurrently (offline)
        a.append_event(&make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Update,
            EventPayload::Update {
                description: Some("A's edit".to_owned()),
                due_date: None,
                completed_on: None,
            },
            "2026-10-07T10:00:00Z",
        ))
        .unwrap();

        b.append_event(&make_event(
            b.device_id().unwrap(),
            todo_uuid,
            EventType::Update,
            EventPayload::Update {
                description: Some("B's edit".to_owned()),
                due_date: None,
                completed_on: None,
            },
            "2026-10-07T11:00:00Z", // later — should win
        ))
        .unwrap();

        exchange_events(&mut a, &mut b);

        let state_a = folded_state(&a);
        let state_b = folded_state(&b);
        assert_eq!(state_a, state_b);
        assert_eq!(state_a.len(), 1);
        // B's edit has a later timestamp → LWW → B wins
        assert_eq!(state_a[0].description, "B's edit");
    }

    #[test]
    fn convergence_delete_vs_update_update_wins_if_later() {
        let mut a = test_database();
        let mut b = test_database();
        let todo_uuid = Uuid::new_v4();

        let create = make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Task".to_owned(),
                due_date: date("2026-10-08"),
            },
            "2026-10-07T09:00:00Z",
        );
        a.append_event(&create).unwrap();
        b.append_event(&create).unwrap();

        // A deletes at T1
        a.append_event(&make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Delete,
            EventPayload::Delete,
            "2026-10-07T10:00:00Z",
        ))
        .unwrap();

        // B updates at T2 > T1
        b.append_event(&make_event(
            b.device_id().unwrap(),
            todo_uuid,
            EventType::Update,
            EventPayload::Update {
                description: Some("Updated".to_owned()),
                due_date: None,
                completed_on: None,
            },
            "2026-10-07T11:00:00Z",
        ))
        .unwrap();

        exchange_events(&mut a, &mut b);

        let state_a = folded_state(&a);
        let state_b = folded_state(&b);
        assert_eq!(state_a, state_b);
        // Update is later → todo survives with updated description
        assert_eq!(state_a.len(), 1);
        assert_eq!(state_a[0].description, "Updated");
    }

    #[test]
    fn convergence_delete_vs_update_delete_wins_if_later() {
        let mut a = test_database();
        let mut b = test_database();
        let todo_uuid = Uuid::new_v4();

        let create = make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Create,
            EventPayload::Create {
                description: "Task".to_owned(),
                due_date: date("2026-10-08"),
            },
            "2026-10-07T09:00:00Z",
        );
        a.append_event(&create).unwrap();
        b.append_event(&create).unwrap();

        // A updates at T1
        a.append_event(&make_event(
            a.device_id().unwrap(),
            todo_uuid,
            EventType::Update,
            EventPayload::Update {
                description: Some("Updated".to_owned()),
                due_date: None,
                completed_on: None,
            },
            "2026-10-07T10:00:00Z",
        ))
        .unwrap();

        // B deletes at T2 > T1
        b.append_event(&make_event(
            b.device_id().unwrap(),
            todo_uuid,
            EventType::Delete,
            EventPayload::Delete,
            "2026-10-07T11:00:00Z",
        ))
        .unwrap();

        exchange_events(&mut a, &mut b);

        let state_a = folded_state(&a);
        let state_b = folded_state(&b);
        assert_eq!(state_a, state_b);
        // Delete is later → todo is gone
        assert!(state_a.is_empty());
    }

    #[test]
    fn convergence_events_deduplicated_after_double_exchange() {
        let mut a = test_database();
        let mut b = test_database();

        a.emit(
            Uuid::new_v4(),
            EventType::Create,
            EventPayload::Create {
                description: "Test".to_owned(),
                due_date: date("2026-10-08"),
            },
        )
        .unwrap();

        let count_before = a.all_events().unwrap().len();

        // Exchange twice — second exchange should add nothing
        exchange_events(&mut a, &mut b);
        exchange_events(&mut a, &mut b);

        let count_after = a.all_events().unwrap().len();
        assert_eq!(count_before, count_after);

        let count_b = b.all_events().unwrap().len();
        assert_eq!(count_before, count_b);
    }

    #[test]
    fn convergence_complex_interleaved_scenario() {
        let mut a = test_database();
        let mut b = test_database();
        let dev_a = a.device_id().unwrap();
        let dev_b = b.device_id().unwrap();

        // Shared todo (synced once before going offline)
        let shared = Uuid::new_v4();
        let create_shared = make_event(
            dev_a,
            shared,
            EventType::Create,
            EventPayload::Create {
                description: "Shared task".to_owned(),
                due_date: date("2026-10-08"),
            },
            "2026-10-07T08:00:00Z",
        );
        a.append_event(&create_shared).unwrap();
        b.append_event(&create_shared).unwrap();

        // A: creates own todo, edits shared, completes shared
        let a_own = Uuid::new_v4();
        a.append_event(&make_event(
            dev_a,
            a_own,
            EventType::Create,
            EventPayload::Create {
                description: "A's private".to_owned(),
                due_date: date("2026-10-09"),
            },
            "2026-10-07T09:00:00Z",
        ))
        .unwrap();
        a.append_event(&make_event(
            dev_a,
            shared,
            EventType::Update,
            EventPayload::Update {
                description: Some("A edited".to_owned()),
                due_date: None,
                completed_on: None,
            },
            "2026-10-07T09:30:00Z",
        ))
        .unwrap();
        a.append_event(&make_event(
            dev_a,
            shared,
            EventType::Update,
            EventPayload::Update {
                description: None,
                due_date: None,
                completed_on: Some(Some(date("2026-10-07"))),
            },
            "2026-10-07T10:00:00Z",
        ))
        .unwrap();

        // B: creates own todo, deletes shared (didn't see A's edits)
        let b_own = Uuid::new_v4();
        b.append_event(&make_event(
            dev_b,
            b_own,
            EventType::Create,
            EventPayload::Create {
                description: "B's private".to_owned(),
                due_date: date("2026-10-10"),
            },
            "2026-10-07T09:15:00Z",
        ))
        .unwrap();
        b.append_event(&make_event(
            dev_b,
            shared,
            EventType::Delete,
            EventPayload::Delete,
            "2026-10-07T09:45:00Z", // before A's complete at 10:00
        ))
        .unwrap();

        exchange_events(&mut a, &mut b);

        let state_a = folded_state(&a);
        let state_b = folded_state(&b);

        // Both machines must have identical state
        assert_eq!(state_a, state_b);

        // Expected: A's private + B's private exist; shared is completed (A's
        // complete at 10:00 is the latest event for `shared`, after B's delete at 09:45)
        assert_eq!(state_a.len(), 3);
        let shared_todo = state_a.iter().find(|t| t.uuid == shared).unwrap();
        assert_eq!(shared_todo.description, "A edited");
        assert_eq!(shared_todo.completed_on, Some(date("2026-10-07")));
    }

}
