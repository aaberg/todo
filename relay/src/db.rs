use rusqlite::{Connection, params};
use uuid::Uuid;

pub struct RelayDb {
    connection: Connection,
}

impl RelayDb {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode = WAL;")?;
        let db = Self { connection };
        db.initialize()?;
        Ok(db)
    }

    fn initialize(&self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY,
                oidc_sub TEXT NOT NULL UNIQUE,
                email TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                token_hash TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(user_id),
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                last_used TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);

            CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL,
                user_id TEXT NOT NULL REFERENCES users(user_id),
                device_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                todo_uuid TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                received_at TEXT NOT NULL,
                UNIQUE(user_id, event_id)
            );
            CREATE INDEX IF NOT EXISTS idx_events_pull
                ON events(user_id, device_id, seq);
            ",
        )?;
        Ok(())
    }

    /// Find or create a user by their OIDC subject.
    pub fn find_or_create_user(
        &self,
        oidc_sub: &str,
        email: Option<&str>,
    ) -> rusqlite::Result<Uuid> {
        if let Ok(user_id) = self.connection.query_row(
            "SELECT user_id FROM users WHERE oidc_sub = ?1",
            params![oidc_sub],
            |row| row.get::<_, String>(0),
        ) {
            return Uuid::parse_str(&user_id)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)));
        }

        let user_id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO users (user_id, oidc_sub, email, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                user_id.to_string(),
                oidc_sub,
                email,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(user_id)
    }

    /// Create a session for a user. Returns the raw token (shown to user once).
    pub fn create_session(
        &mut self,
        user_id: Uuid,
        ttl_secs: i64,
    ) -> rusqlite::Result<(String, String)> {
        let token = Uuid::new_v4().to_string() + &Uuid::new_v4().to_string().replace('-', "");
        let token_hash = hash_token(&token);
        let now = chrono::Utc::now();
        let expires = now + chrono::Duration::seconds(ttl_secs);

        self.connection.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                &token_hash,
                user_id.to_string(),
                now.to_rfc3339(),
                expires.to_rfc3339(),
            ],
        )?;
        Ok((token, token_hash))
    }

    /// Validate a session token. Returns the user_id if valid.
    /// Updates last_used on success.
    pub fn validate_session(&self, token: &str) -> rusqlite::Result<Option<Uuid>> {
        let token_hash = hash_token(token);
        let result = self.connection.query_row(
            "SELECT user_id, expires_at FROM sessions WHERE token_hash = ?1",
            params![&token_hash],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                ))
            },
        );

        match result {
            Ok((user_id_str, expires_at)) => {
                let expires = chrono::DateTime::parse_from_rfc3339(&expires_at)
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?
                    .with_timezone(&chrono::Utc);
                if expires < chrono::Utc::now() {
                    return Ok(None);
                }
                // Update last_used
                self.connection.execute(
                    "UPDATE sessions SET last_used = ?1 WHERE token_hash = ?2",
                    params![chrono::Utc::now().to_rfc3339(), &token_hash],
                )?;
                Uuid::parse_str(&user_id_str)
                    .map(Some)
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete a session (logout).
    pub fn delete_session(&self, token: &str) -> rusqlite::Result<()> {
        let token_hash = hash_token(token);
        self.connection.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            params![&token_hash],
        )?;
        Ok(())
    }

    /// Insert events, deduplicating by (user_id, event_id). Returns count of newly inserted.
    pub fn insert_events(
        &mut self,
        user_id: Uuid,
        events: &[todo_common::wire::WireEvent],
    ) -> rusqlite::Result<usize> {
        let mut accepted = 0;
        for event in events {
            let result = self.connection.execute(
                "INSERT OR IGNORE INTO events
                    (event_id, user_id, device_id, timestamp, todo_uuid, event_type, payload, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.event_id.to_string(),
                    user_id.to_string(),
                    event.device_id.to_string(),
                    event.timestamp.to_rfc3339(),
                    event.todo_uuid.to_string(),
                    &event.event_type,
                    &event.payload.to_string(),
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
            accepted += result;
        }
        Ok(accepted)
    }

    /// Pull events for a user, excluding their own device, since a given seq.
    pub fn pull_events(
        &self,
        user_id: Uuid,
        exclude_device: Uuid,
        since: i64,
        limit: i64,
    ) -> rusqlite::Result<Vec<todo_common::wire::PulledEvent>> {
        let mut stmt = self.connection.prepare(
            "SELECT seq, event_id, device_id, timestamp, todo_uuid, event_type, payload
             FROM events
             WHERE user_id = ?1 AND device_id != ?2 AND seq > ?3
             ORDER BY seq
             LIMIT ?4",
        )?;

        let rows = stmt.query_map(
            params![user_id.to_string(), exclude_device.to_string(), since, limit],
            |row| {
                Ok(todo_common::wire::PulledEvent {
                    seq: row.get(0)?,
                    event: todo_common::wire::WireEvent {
                        event_id: parse_uuid(&row.get::<_, String>(1)?)?,
                        device_id: parse_uuid(&row.get::<_, String>(2)?)?,
                        timestamp: parse_datetime(&row.get::<_, String>(3)?)?,
                        todo_uuid: parse_uuid(&row.get::<_, String>(4)?)?,
                        event_type: row.get(5)?,
                        payload: serde_json::from_str(&row.get::<_, String>(6)?).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                    },
                })
            },
        )?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Get the latest seq for a user (for the pull response).
    pub fn latest_seq(&self, user_id: Uuid) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE user_id = ?1",
            params![user_id.to_string()],
            |row| row.get(0),
        )
    }

    /// Get user email for display.
    pub fn user_email(&self, user_id: Uuid) -> rusqlite::Result<Option<String>> {
        match self.connection.query_row(
            "SELECT email FROM users WHERE user_id = ?1",
            params![user_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(email) => Ok(email),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn parse_uuid(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_datetime(value: &str) -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}
