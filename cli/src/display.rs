use chrono::NaiveDate;

use crate::event::Event;
use crate::model::{Todo, group_name};

const ORANGE: &str = "\x1b[38;2;255;165;0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const STRIKETHROUGH: &str = "\x1b[9m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

pub fn print_list(todos: &[(i64, Todo)], today: NaiveDate) {
    if todos.is_empty() {
        println!("No todos.");
        return;
    }

    let mut current_group: Option<String> = None;
    for (id, todo) in todos {
        let group = group_name(todo, today);
        if current_group.as_deref() != Some(group.as_str()) {
            if current_group.is_some() {
                println!();
            }
            print_group_heading(&group);
            current_group = Some(group);
        }

        print_todo(*id, todo, today);
    }
}

fn print_group_heading(group: &str) {
    if group == "Today" {
        println!("{ORANGE}{BOLD}{group}{RESET}");
    } else {
        println!("{ORANGE}{group}{RESET}");
    }
}

fn print_todo(id: i64, todo: &Todo, today: NaiveDate) {
    let state = if todo.completed_on.is_some() {
        "x"
    } else {
        " "
    };
    let text = format!("  [{state}] {id} {}", todo.description);

    if todo.completed_on.is_some() {
        println!("{GREEN}{STRIKETHROUGH}{text}{RESET}");
    } else if todo.due_date < today {
        println!("{RED}{text}{RESET}");
    } else {
        println!("{text}");
    }
}

pub fn print_log(id: i64, todo: &Todo, events: &[Event]) {
    println!("{BOLD}Todo {id}: {}{RESET}", todo.description);
    println!("{DIM}UUID: {}{RESET}", todo.uuid);
    println!("{DIM}Due: {}{RESET}", todo.due_date.format("%Y-%m-%d"));
    match todo.completed_on {
        Some(date) => println!("{DIM}Completed: {}{RESET}", date.format("%Y-%m-%d")),
        None => println!("{DIM}Not completed{RESET}"),
    }
    println!();
    println!("{BOLD}History{RESET}");
    for event in events {
        let ts = event.timestamp.format("%Y-%m-%d %H:%M:%S UTC");
        let device = &event.device_id.to_string()[..8];
        let description = match &event.payload {
            crate::event::EventPayload::Create { description, due_date } => {
                format!("created: \"{description}\" due {}", due_date.format("%Y-%m-%d"))
            }
            crate::event::EventPayload::Update {
                description,
                due_date,
                completed_on,
            } => {
                let mut parts = Vec::new();
                if let Some(d) = description {
                    parts.push(format!("description → \"{d}\""));
                }
                if let Some(d) = due_date {
                    parts.push(format!("due → {}", d.format("%Y-%m-%d")));
                }
                if let Some(c) = completed_on {
                    match c {
                        Some(date) => parts.push(format!("completed → {}", date.format("%Y-%m-%d"))),
                        None => parts.push("completed → cleared".to_owned()),
                    }
                }
                format!("updated: {}", parts.join(", "))
            }
            crate::event::EventPayload::Delete => "deleted".to_owned(),
        };
        println!("  {DIM}{ts}{RESET} [{device}] {description}");
    }
}
