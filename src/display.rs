use chrono::NaiveDate;

use crate::model::{Todo, group_name};

const ORANGE: &str = "\x1b[38;2;255;165;0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const STRIKETHROUGH: &str = "\x1b[9m";
const RESET: &str = "\x1b[0m";

pub fn print_list(todos: &[Todo], today: NaiveDate) {
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
