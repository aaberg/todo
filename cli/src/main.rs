mod callback;
mod cli;
mod config;
mod date;
mod db;
mod display;
mod event;
mod model;
mod sync;

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
use model::{fold_events, prepare_for_display, resolve_display_id, sort_for_display, with_display_ids, Todo};

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
            let display = prepare_for_display(todos, today, all);
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
        Commands::Login { relay } => {
            let relay_url = match relay {
                Some(url) => url,
                None => {
                    let config = config::Config::load();
                    let saved = config.sync_url.filter(|url| !url.trim().is_empty());
                    prompt_relay_url(saved.as_deref())?
                }
            };
            sync::login(&relay_url)?;
        }
        Commands::Sync => {
            let config = config::Config::load();
            let stats = sync::sync(&mut db, &config)?;
            println!("Synced: pulled {} event(s), pushed {} event(s).", stats.pulled, stats.pushed);
        }
        Commands::Logout => {
            let mut config = config::Config::load();
            sync::logout(&mut config)?;
            println!("Logged out.");
        }
        Commands::Whoami => {
            let config = config::Config::load();
            let me = sync::whoami(&config)?;
            let relay = config
                .sync_url
                .as_deref()
                .map(sanitize_url_for_display)
                .unwrap_or_else(|| "<unknown>".to_string());
            match me.email {
                Some(email) => println!("Logged in as {email} on relay {relay}"),
                None => println!("Logged in (user {}) on relay {relay}", me.user_id),
            }
        }
    }

    Ok(())
}

/// Make a URL safe to print: strip userinfo and control characters so a
/// legacy config value cannot leak a password or corrupt terminal output.
fn sanitize_url_for_display(url: &str) -> String {
    let cleaned: String = url.chars().filter(|c| !c.is_control()).collect();

    match url::Url::parse(&cleaned) {
        Ok(parsed) => {
            if parsed.query().is_some() || parsed.fragment().is_some() {
                return "<invalid saved relay URL>".to_string();
            }
            let mut safe = parsed;
            safe.set_username("").ok();
            safe.set_password(None).ok();
            safe.into()
        }
        // Unparseable legacy value: if it might embed userinfo ("@"), fall
        // back to a placeholder rather than risk printing credentials.
        Err(_) => {
            if cleaned.contains('@') {
                "<invalid saved relay URL>".to_string()
            } else {
                cleaned
            }
        }
    }
}

/// Ask the user for the sync relay URL.
///
/// Shows an example of the expected format. If a URL was saved by a previous
/// login, it is offered as the default so pressing Enter reuses it.
fn prompt_relay_url(saved_url: Option<&str>) -> Result<String, Box<dyn Error>> {
    let prompt = match saved_url {
        Some(url) => format!("Relay URL [default: {}]: ", sanitize_url_for_display(url)),
        None => "Relay URL (e.g. https://relay.example.com): ".to_string(),
    };
    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return match saved_url {
            Some(url) => Ok(url.to_string()),
            None => Err("no relay URL given — run `todo login --relay <url>` or enter a URL when prompted".into()),
        };
    }
    Ok(input.to_string())
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

#[cfg(test)]
mod tests {
    use super::sanitize_url_for_display;

    #[test]
    fn sanitize_url_strips_userinfo() {
        assert_eq!(
            sanitize_url_for_display("https://user:secret@relay.example.com"),
            "https://relay.example.com/"
        );
        assert_eq!(
            sanitize_url_for_display("https://user@relay.example.com"),
            "https://relay.example.com/"
        );
    }

    #[test]
    fn sanitize_url_redacts_userinfo_in_unparseable_values() {
        assert_eq!(
            sanitize_url_for_display("https://user:secret@relay with space"),
            "<invalid saved relay URL>"
        );
        assert_eq!(
            sanitize_url_for_display("just some not-url text"),
            "just some not-url text"
        );
    }

    #[test]
    fn sanitize_url_keeps_plain_urls_and_strips_control_chars() {
        assert_eq!(
            sanitize_url_for_display("https://relay.example.com"),
            "https://relay.example.com/"
        );
        assert_eq!(
            sanitize_url_for_display("https://relay.example.com\u{7}"),
            "https://relay.example.com/"
        );
    }
}
