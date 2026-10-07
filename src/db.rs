use std::path::PathBuf;

use chrono::NaiveDate;
use rusqlite::{Connection, params};

use crate::model::{Todo, group_order, todo_order_within_group};

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        let mut database = Self { connection };
        database.initialize()?;
        Ok(database)
    }

    fn initialize(&mut self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                description TEXT NOT NULL,
                due_date TEXT NOT NULL,
                completed_on TEXT
            );
            ",
        )
    }

    pub fn add(&mut self, description: &str, due_date: NaiveDate) -> rusqlite::Result<i64> {
        self.connection.execute(
            "INSERT INTO todos (description, due_date) VALUES (?1, ?2)",
            params![description, due_date.format("%Y-%m-%d").to_string()],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn set_completed(
        &mut self,
        id: i64,
        completed_on: Option<NaiveDate>,
    ) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE todos SET completed_on = ?1 WHERE id = ?2",
            params![
                completed_on.map(|date| date.format("%Y-%m-%d").to_string()),
                id
            ],
        )
    }

    pub fn edit(&mut self, id: i64, description: &str) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE todos SET description = ?1 WHERE id = ?2",
            params![description, id],
        )
    }

    pub fn list(&self, all: bool, today: NaiveDate) -> rusqlite::Result<Vec<Todo>> {
        let mut statement = if all {
            self.connection.prepare(
                "SELECT id, description, due_date, completed_on
                 FROM todos
                 ORDER BY due_date, id",
            )?
        } else {
            self.connection.prepare(
                "SELECT id, description, due_date, completed_on
                 FROM todos
                 WHERE completed_on IS NULL OR completed_on = ?1
                 ORDER BY due_date, id",
            )?
        };

        let mut todos = Vec::new();
        let mut rows = if all {
            statement.query([])?
        } else {
            statement.query(params![today.format("%Y-%m-%d").to_string()])?
        };
        while let Some(row) = rows.next()? {
            todos.push(Todo {
                id: row.get(0)?,
                description: row.get(1)?,
                due_date: date_from_row(row.get::<_, String>(2)?)?,
                completed_on: row
                    .get::<_, Option<String>>(3)?
                    .map(date_from_row)
                    .transpose()?,
            });
        }
        todos.sort_by_key(|todo| {
            (
                group_order(todo, today),
                todo_order_within_group(todo, today),
                todo.due_date,
                todo.id,
            )
        });
        Ok(todos)
    }

    pub fn prune(&mut self) -> rusqlite::Result<usize> {
        self.connection
            .execute("DELETE FROM todos WHERE completed_on IS NOT NULL", [])
    }
}

fn date_from_row(value: String) -> rusqlite::Result<NaiveDate> {
    NaiveDate::parse_from_str(&value, "%Y-%m-%d").map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::group_name;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn test_database() -> Database {
        let connection = Connection::open_in_memory().unwrap();
        let mut database = Database { connection };
        database.initialize().unwrap();
        database
    }

    #[test]
    fn default_list_keeps_open_and_today_completed_todos() {
        let today = date("2026-08-11");
        let mut database = test_database();
        let open_id = database.add("open", today).unwrap();
        let completed_today = database.add("finished today", today).unwrap();
        let completed_before = database.add("old finished", today).unwrap();
        database
            .set_completed(completed_today, Some(today))
            .unwrap();
        database
            .set_completed(completed_before, Some(date("2026-08-10")))
            .unwrap();

        let todos = database.list(false, today).unwrap();
        assert_eq!(
            todos.iter().map(|todo| todo.id).collect::<Vec<_>>(),
            [open_id, completed_today]
        );
        assert_eq!(database.list(true, today).unwrap().len(), 3);
    }

    #[test]
    fn pruning_deletes_only_completed_todos() {
        let today = date("2026-08-11");
        let mut database = test_database();
        let completed = database.add("finished", today).unwrap();
        database.add("open", today).unwrap();
        database.set_completed(completed, Some(today)).unwrap();

        assert_eq!(database.prune().unwrap(), 1);
        assert_eq!(database.list(true, today).unwrap().len(), 1);
    }

    #[test]
    fn editing_updates_only_the_description() {
        let today = date("2026-08-11");
        let due_date = date("2026-08-13");
        let mut database = test_database();
        let id = database.add("original", due_date).unwrap();
        database.set_completed(id, Some(today)).unwrap();

        assert_eq!(database.edit(id, "updated").unwrap(), 1);

        let todo = database.list(true, today).unwrap().pop().unwrap();
        assert_eq!(todo.description, "updated");
        assert_eq!(todo.due_date, due_date);
        assert_eq!(todo.completed_on, Some(today));
    }

    #[test]
    fn editing_a_missing_todo_updates_nothing() {
        let mut database = test_database();

        assert_eq!(database.edit(12, "updated").unwrap(), 0);
    }

    #[test]
    fn all_list_keeps_each_group_contiguous() {
        let today = date("2026-08-11");
        let mut database = test_database();
        let historical_done = database.add("historical", date("2026-08-09")).unwrap();
        database.add("overdue", date("2026-08-10")).unwrap();
        database.add("today", today).unwrap();
        database
            .set_completed(historical_done, Some(today))
            .unwrap();

        let groups = database
            .list(true, today)
            .unwrap()
            .iter()
            .map(|todo| group_name(todo, today))
            .collect::<Vec<_>>();
        assert_eq!(groups, ["Today", "Today", "2026-08-09"]);
    }

    #[test]
    fn today_group_orders_overdue_then_due_then_done() {
        let today = date("2026-08-11");
        let mut database = test_database();
        let done = database.add("done", today).unwrap();
        let overdue = database.add("overdue", date("2026-08-10")).unwrap();
        let due = database.add("due", today).unwrap();
        database.set_completed(done, Some(today)).unwrap();

        let todos = database.list(false, today).unwrap();
        assert_eq!(
            todos.iter().map(|todo| todo.id).collect::<Vec<_>>(),
            [overdue, due, done]
        );
    }
}
