mod cli;
mod date;
mod db;
mod display;
mod event;
mod model;

use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    path::PathBuf,
};

use chrono::Local;
use clap::Parser;
use uuid::Uuid;

use cli::{Cli, Commands};
use date::parse_relative_due_date;
use db::Database;
use display::{print_list, print_log};
use event::{EventPayload, EventType};
use model::{fold_events, resolve_display_id, sort_for_display, with_display_ids, Todo};

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

    // Fold the event log into current state.
    // For read-only commands (list, log) this is all we need.
    // For mutating commands, we resolve the display ID then append a new event.
    let events = db.all_events()?;
    let todos = fold_events(&events);

    match cli.command {
        Commands::List { all } => {
            let mut display = with_display_ids(todos);
            if !all {
                display.retain(|(_, t)| {
                    t.completed_on.is_none() || t.completed_on == Some(today)
                });
            }
            sort_for_display(&mut display, today);
            print_list(&display, today);
        }
        Commands::Add { description, due } => {
            let due_date = match due {
                Some(value) => parse_relative_due_date(&value, today)?,
                None => today,
            };
            let todo_uuid = Uuid::new_v4();
            db.emit(
                todo_uuid,
                EventType::Create,
                EventPayload::Create {
                    description: description.clone(),
                    due_date,
                },
            )?;
            println!("Added todo for {}.", due_date.format("%Y-%m-%d"));
        }
        Commands::Edit { id, description } => {
            let mut display = with_display_ids(todos);
            sort_for_display(&mut display, today);
            let todo_uuid = resolve_display_id(id, &display)
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            db.emit(
                todo_uuid,
                EventType::Update,
                EventPayload::Update {
                    description: Some(description.clone()),
                    due_date: None,
                    completed_on: None,
                },
            )?;
            println!("Updated todo {id}.");
        }
        Commands::Done { id } => {
            let mut display = with_display_ids(todos);
            sort_for_display(&mut display, today);
            let todo_uuid = resolve_display_id(id, &display)
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            db.emit(
                todo_uuid,
                EventType::Update,
                EventPayload::Update {
                    description: None,
                    due_date: None,
                    completed_on: Some(Some(today)),
                },
            )?;
            println!("Marked todo {id} as done.");
        }
        Commands::Undone { id } => {
            let mut display = with_display_ids(todos);
            sort_for_display(&mut display, today);
            let todo_uuid = resolve_display_id(id, &display)
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            db.emit(
                todo_uuid,
                EventType::Update,
                EventPayload::Update {
                    description: None,
                    due_date: None,
                    completed_on: Some(None),
                },
            )?;
            println!("Marked todo {id} as not done.");
        }
        Commands::Remove { id } => {
            let mut display = with_display_ids(todos);
            sort_for_display(&mut display, today);
            let todo_uuid = resolve_display_id(id, &display)
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            db.emit(todo_uuid, EventType::Delete, EventPayload::Delete)?;
            println!("Removed todo {id}.");
        }
        Commands::Log { id } => {
            let mut display = with_display_ids(todos);
            sort_for_display(&mut display, today);
            let todo_uuid = resolve_display_id(id, &display)
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            let todo = display
                .iter()
                .find(|(did, _)| *did == id)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| format!("todo {id} does not exist"))?;
            let todo_events = db.events_for_todo(todo_uuid)?;
            print_log(id, &todo, &todo_events);
        }
        Commands::Prune => prune(&mut db, &todos)?,
    }

    Ok(())
}

fn default_database_path() -> Result<PathBuf, Box<dyn Error>> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    let directory = PathBuf::from(home).join(".todo");
    fs::create_dir_all(&directory)?;
    Ok(directory.join("todos.db"))
}

fn prune(db: &mut Database, todos: &[Todo]) -> Result<(), Box<dyn Error>> {
    // Prune all completed todos — deletion is now a tombstone event, so
    // "completed today" todos are included just like any other.
    let completed: Vec<_> = todos
        .iter()
        .filter(|t| t.completed_on.is_some())
        .collect();


    if completed.is_empty() {
        println!("No completed todos to prune.");
        return Ok(());
    }

    print!(
        "Permanently delete {} completed todo(s)? [y/N] ",
        completed.len()
    );
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    if !matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        println!("Aborted.");
        return Ok(());
    }

    for todo in &completed {
        db.emit(todo.uuid, EventType::Delete, EventPayload::Delete)?;
    }
    println!("Pruned {} todo(s).", completed.len());
    Ok(())
}
