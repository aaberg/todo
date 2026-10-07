use chrono::NaiveDate;

#[derive(Debug, PartialEq, Eq)]
pub struct Todo {
    pub id: i64,
    pub description: String,
    pub due_date: NaiveDate,
    pub completed_on: Option<NaiveDate>,
}

pub fn group_name(todo: &Todo, today: NaiveDate) -> String {
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

pub fn group_order(todo: &Todo, today: NaiveDate) -> u8 {
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

pub fn todo_order_within_group(todo: &Todo, today: NaiveDate) -> u8 {
    if todo.completed_on.is_some() {
        2
    } else if todo.due_date < today {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
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
}
