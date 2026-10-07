mod cli;
mod date;
mod db;
mod display;
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

use cli::{Cli, Commands};
use date::parse_relative_due_date;
use db::Database;
use display::print_list;

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
