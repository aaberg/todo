use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    path::PathBuf,
};

use chrono::{Local, NaiveDate};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, params};

const ORANGE: &str = "\x1b[38;2;255;165;0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const STRIKETHROUGH: &str = "\x1b[9m";
const RESET: &str = "\x1b[0m";

#[derive(Parser)]
#[command(
    name = "todo",
    about = "A small, local todo list",
    version,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show all relevant todos
    List {
        /// Include completed todos from every day
        #[arg(short = 'a', long = "all")]
        all: bool,
    },
    /// Add a todo due today or after a relative number of days (for example, 1d)
    Add {
        /// Description of the todo
        description: String,
        /// Relative due date, such as 1d for tomorrow
        due: Option<String>,
    },
    /// Change a todo's description
    Edit {
        id: i64,
        /// New description of the todo
        description: String,
    },
    /// Mark a todo as done
    Done { id: i64 },
    /// Mark a todo as not done
    Undone { id: i64 },
    /// Permanently delete all completed todos
    Prune,
}

#[derive(Debug, PartialEq, Eq)]
struct Todo {
    id: i64,
    description: String,
    due_date: NaiveDate,
    completed_on: Option<NaiveDate>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let today = Local::now().date_naive();

    let mut db = Database::open(default_database_path()?)?;
    match cli.command {
        Commands::List { all } => {
            let todos = db.list(all, today)?;
            print_list(&todos, today);
        }
        Commands::Add { description, due } => {
            let due_date = match due {
                Some(value) => parse_relative_due_date(&value, today)?,
                None => today,
            };
            let id = db.add(&description, due_date)?;
            println!("Added todo {id} for {}.", due_date.format("%Y-%m-%d"));
        }
        Commands::Edit { id, description } => {
            ensure_updated(db.edit(id, &description)?, id)?;
            println!("Updated todo {id}.");
        }
        Commands::Done { id } => {
            ensure_updated(db.set_completed(id, Some(today))?, id)?;
            println!("Marked todo {id} as done.");
        }
        Commands::Undone { id } => {
            ensure_updated(db.set_completed(id, None)?, id)?;
            println!("Marked todo {id} as not done.");
        }
        Commands::Prune => prune(&mut db)?,
    }

    Ok(())
}

fn default_database_path() -> Result<PathBuf, Box<dyn Error>> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    let directory = PathBuf::from(home).join(".todo");
    fs::create_dir_all(&directory)?;
    Ok(directory.join("todos.db"))
}

fn parse_relative_due_date(value: &str, today: NaiveDate) -> Result<NaiveDate, Box<dyn Error>> {
    let days = value
        .strip_suffix('d')
        .ok_or("due date must be a number of days followed by d, for example 1d")?
        .parse::<i64>()?;

    if days < 0 {
        return Err("due date cannot be negative".into());
    }

    today
        .checked_add_signed(chrono::TimeDelta::days(days))
        .ok_or_else(|| "due date is out of range".into())
}

fn ensure_updated(updated: usize, id: i64) -> Result<(), Box<dyn Error>> {
    if updated == 0 {
        return Err(format!("todo {id} does not exist").into());
    }
    Ok(())
}

fn prune(db: &mut Database) -> Result<(), Box<dyn Error>> {
    print!("Permanently delete all completed todos? [y/N] ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    if !matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        println!("Aborted.");
        return Ok(());
    }

    let deleted = db.prune()?;
    println!("Pruned {deleted} todo(s).");
    Ok(())
}

fn print_list(todos: &[Todo], today: NaiveDate) {
    if todos.is_empty() {
        println!("No todos.");
        return;
    }

    let mut current_group: Option<String> = None;
    for todo in todos {
        let group = group_name(todo, today);
        if current_group.as_deref() != Some(group.as_str()) {
            if current_group.is_some() {
                println!();
            }
            print_group_heading(&group);
            current_group = Some(group);
        }

        print_todo(todo, today);
    }
}

fn group_name(todo: &Todo, today: NaiveDate) -> String {
    if todo.completed_on.is_none() && todo.due_date <= today {
        "Today".to_owned()
    } else if todo.due_date == today {
        "Today".to_owned()
    } else if todo.due_date == today.succ_opt().expect("valid tomorrow") {
        "Tomorrow".to_owned()
    } else {
        todo.due_date.format("%Y-%m-%d").to_string()
    }
}

fn print_group_heading(group: &str) {
    if group == "Today" {
        println!("{ORANGE}{BOLD}{group}{RESET}");
    } else {
        println!("{ORANGE}{group}{RESET}");
    }
}

fn print_todo(todo: &Todo, today: NaiveDate) {
    let state = if todo.completed_on.is_some() {
        "x"
    } else {
        " "
    };
    let text = format!("  [{state}] {} {}", todo.id, todo.description);

    if todo.completed_on.is_some() {
        println!("{GREEN}{STRIKETHROUGH}{text}{RESET}");
    } else if todo.due_date < today {
        println!("{RED}{text}{RESET}");
    } else {
        println!("{text}");
    }
}

struct Database {
    connection: Connection,
}

impl Database {
    fn open(path: PathBuf) -> rusqlite::Result<Self> {
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

    fn add(&mut self, description: &str, due_date: NaiveDate) -> rusqlite::Result<i64> {
        self.connection.execute(
            "INSERT INTO todos (description, due_date) VALUES (?1, ?2)",
            params![description, due_date.format("%Y-%m-%d").to_string()],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    fn set_completed(
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

    fn edit(&mut self, id: i64, description: &str) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE todos SET description = ?1 WHERE id = ?2",
            params![description, id],
        )
    }

    fn list(&self, all: bool, today: NaiveDate) -> rusqlite::Result<Vec<Todo>> {
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

    fn prune(&mut self) -> rusqlite::Result<usize> {
        self.connection
            .execute("DELETE FROM todos WHERE completed_on IS NOT NULL", [])
    }
}

fn group_order(todo: &Todo, today: NaiveDate) -> u8 {
    if group_name(todo, today) == "Today" {
        0
    } else if group_name(todo, today) == "Tomorrow" {
        1
    } else if todo.due_date > today {
        2
    } else {
        3
    }
}

fn todo_order_within_group(todo: &Todo, today: NaiveDate) -> u8 {
    if todo.completed_on.is_some() {
        2
    } else if todo.due_date < today {
        0
    } else {
        1
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
    fn relative_due_dates_are_calculated_from_today() {
        let today = date("2026-08-11");
        assert_eq!(
            parse_relative_due_date("1d", today).unwrap(),
            date("2026-08-12")
        );
        assert_eq!(parse_relative_due_date("0d", today).unwrap(), today);
        assert!(parse_relative_due_date("tomorrow", today).is_err());
        assert!(parse_relative_due_date("-1d", today).is_err());
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
    fn overdue_open_todos_are_grouped_with_today() {
        let today = date("2026-08-11");
        let todo = Todo {
            id: 1,
            description: "overdue".to_owned(),
            due_date: date("2026-08-10"),
            completed_on: None,
        };
        assert_eq!(group_name(&todo, today), "Today");
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
