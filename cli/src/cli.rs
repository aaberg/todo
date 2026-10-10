use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "todo",
    about = "A small, local todo list",
    version,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
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
    /// Permanently delete a todo
    Remove { id: i64 },
    /// Show the event history for a todo
    Log { id: i64 },
    /// Permanently delete all completed todos
    Prune,
    /// Log in to the sync relay (opens browser)
    Login {
        /// Relay URL, e.g. https://relay.example.com
        #[arg(long)]
        relay: Option<String>,
    },
    /// Synchronize todos with the relay
    Sync,
    /// Log out and revoke the session
    Logout,
    /// Show the currently logged-in user
    Whoami,
}
