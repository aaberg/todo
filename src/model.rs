use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use uuid::Uuid;

use crate::event::{Event, EventPayload, EventType};

/// A todo as materialized from the event log.
/// No local id — display IDs are derived from list position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub uuid: Uuid,
    pub description: String,
    pub due_date: NaiveDate,
    pub completed_on: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
}

/// Fold the event log into current todo state.
/// Events MUST be pre-sorted by (timestamp, event_id) for deterministic order.
/// Returns todos in creation order (stable for display ID assignment).
pub fn fold_events(events: &[Event]) -> Vec<Todo> {
    let mut map: HashMap<Uuid, Todo> = HashMap::new();
    let mut order: Vec<Uuid> = Vec::new();

    for event in events {
        match event.event_type {
            EventType::Create => {
                if let EventPayload::Create { description, due_date } = &event.payload {
                    let todo = Todo {
                        uuid: event.todo_uuid,
                        description: description.clone(),
                        due_date: *due_date,
                        completed_on: None,
                        created_at: event.timestamp,
                    };
                    if !map.contains_key(&event.todo_uuid) {
                        order.push(event.todo_uuid);
                    }
                    map.insert(event.todo_uuid, todo);
                }
            }
            EventType::Update => {
                if let Some(todo) = map.get_mut(&event.todo_uuid) {
                    if let EventPayload::Update {
                        description,
                        due_date,
                        completed_on,
                    } = &event.payload
                    {
                        if let Some(d) = description {
                            todo.description = d.clone();
                        }
                        if let Some(d) = due_date {
                            todo.due_date = *d;
                        }
                        if let Some(c) = completed_on {
                            todo.completed_on = *c;
                        }
                    }
                }
            }
            EventType::Delete => {
                map.remove(&event.todo_uuid);
                order.retain(|u| *u != event.todo_uuid);
            }
        }
    }

    order
        .into_iter()
        .filter_map(|uuid| map.remove(&uuid))
        .collect()
}

/// Assign 1-based display IDs based on list position.
/// Returns Vec<(display_id, Todo)> preserving order.
pub fn with_display_ids(todos: Vec<Todo>) -> Vec<(i64, Todo)> {
    todos
        .into_iter()
        .enumerate()
        .map(|(i, todo)| (i as i64 + 1, todo))
        .collect()
}

/// Resolve a user-supplied display ID to a todo UUID.
/// Returns None if the ID is out of range.
pub fn resolve_display_id(display_id: i64, todos: &[(i64, Todo)]) -> Option<Uuid> {
    todos
        .iter()
        .find(|(id, _)| *id == display_id)
        .map(|(_, todo)| todo.uuid)
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

/// Sort todos for display: by group, then within-group order, then due date, then creation.
pub fn sort_for_display(todos: &mut [(i64, Todo)], today: NaiveDate) {
    todos.sort_by_key(|(_, todo)| {
        (
            group_order(todo, today),
            todo_order_within_group(todo, today),
            todo.due_date,
            todo.created_at,
        )
    });
    // Re-assign display IDs after sorting
    for (i, (id, _)) in todos.iter_mut().enumerate() {
        *id = i as i64 + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventPayload;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn create_event(device: Uuid, todo: Uuid, description: &str, due: &str, ts: &str) -> Event {
        Event {
            event_id: Uuid::new_v4(),
            device_id: device,
            timestamp: DateTime::parse_from_rfc3339(ts).unwrap().with_timezone(&Utc),
            todo_uuid: todo,
            event_type: EventType::Create,
            payload: EventPayload::Create {
                description: description.to_owned(),
                due_date: date(due),
            },
        }
    }

    fn update_event(device: Uuid, todo: Uuid, ts: &str, payload: EventPayload) -> Event {
        Event {
            event_id: Uuid::new_v4(),
            device_id: device,
            timestamp: DateTime::parse_from_rfc3339(ts).unwrap().with_timezone(&Utc),
            todo_uuid: todo,
            event_type: EventType::Update,
            payload,
        }
    }

    fn delete_event(device: Uuid, todo: Uuid, ts: &str) -> Event {
        Event {
            event_id: Uuid::new_v4(),
            device_id: device,
            timestamp: DateTime::parse_from_rfc3339(ts).unwrap().with_timezone(&Utc),
            todo_uuid: todo,
            event_type: EventType::Delete,
            payload: EventPayload::Delete,
        }
    }

    #[test]
    fn fold_create_produces_todo() {
        let device = Uuid::new_v4();
        let todo_uuid = Uuid::new_v4();
        let events = vec![create_event(device, todo_uuid, "Buy milk", "2026-10-08", "2026-10-07T10:00:00Z")];

        let todos = fold_events(&events);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].description, "Buy milk");
        assert_eq!(todos[0].due_date, date("2026-10-08"));
        assert!(todos[0].completed_on.is_none());
    }

    #[test]
    fn fold_update_changes_fields() {
        let device = Uuid::new_v4();
        let todo_uuid = Uuid::new_v4();
        let events = vec![
            create_event(device, todo_uuid, "Buy milk", "2026-10-08", "2026-10-07T10:00:00Z"),
            update_event(
                device,
                todo_uuid,
                "2026-10-07T11:00:00Z",
                EventPayload::Update {
                    description: Some("Buy oat milk".to_owned()),
                    due_date: None,
                    completed_on: None,
                },
            ),
        ];

        let todos = fold_events(&events);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].description, "Buy oat milk");
        assert_eq!(todos[0].due_date, date("2026-10-08"));
    }

    #[test]
    fn fold_update_marks_completed_and_undone() {
        let device = Uuid::new_v4();
        let todo_uuid = Uuid::new_v4();
        let events = vec![
            create_event(device, todo_uuid, "Task", "2026-10-08", "2026-10-07T10:00:00Z"),
            update_event(
                device,
                todo_uuid,
                "2026-10-07T11:00:00Z",
                EventPayload::Update {
                    description: None,
                    due_date: None,
                    completed_on: Some(Some(date("2026-10-07"))),
                },
            ),
            update_event(
                device,
                todo_uuid,
                "2026-10-07T12:00:00Z",
                EventPayload::Update {
                    description: None,
                    due_date: None,
                    completed_on: Some(None),
                },
            ),
        ];

        let todos = fold_events(&events);
        assert_eq!(todos.len(), 1);
        assert!(todos[0].completed_on.is_none());
    }

    #[test]
    fn fold_delete_removes_todo() {
        let device = Uuid::new_v4();
        let todo_uuid = Uuid::new_v4();
        let events = vec![
            create_event(device, todo_uuid, "Task", "2026-10-08", "2026-10-07T10:00:00Z"),
            delete_event(device, todo_uuid, "2026-10-07T11:00:00Z"),
        ];

        let todos = fold_events(&events);
        assert!(todos.is_empty());
    }

    #[test]
    fn fold_multiple_todos_preserves_creation_order() {
        let device = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let events = vec![
            create_event(device, a, "First", "2026-10-08", "2026-10-07T10:00:00Z"),
            create_event(device, b, "Second", "2026-10-09", "2026-10-07T10:01:00Z"),
        ];

        let todos = fold_events(&events);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].description, "First");
        assert_eq!(todos[1].description, "Second");
    }

    #[test]
    fn fold_delete_then_recreate_keeps_new_instance() {
        let device = Uuid::new_v4();
        let todo_uuid = Uuid::new_v4();
        let events = vec![
            create_event(device, todo_uuid, "Original", "2026-10-08", "2026-10-07T10:00:00Z"),
            delete_event(device, todo_uuid, "2026-10-07T11:00:00Z"),
            create_event(device, todo_uuid, "Recreated", "2026-10-09", "2026-10-07T12:00:00Z"),
        ];

        let todos = fold_events(&events);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].description, "Recreated");
    }

    #[test]
    fn display_ids_are_sequential() {
        let todos = vec![
            Todo {
                uuid: Uuid::new_v4(),
                description: "a".to_owned(),
                due_date: date("2026-10-08"),
                completed_on: None,
                created_at: Utc::now(),
            },
            Todo {
                uuid: Uuid::new_v4(),
                description: "b".to_owned(),
                due_date: date("2026-10-08"),
                completed_on: None,
                created_at: Utc::now(),
            },
        ];
        let with_ids = with_display_ids(todos);
        assert_eq!(with_ids[0].0, 1);
        assert_eq!(with_ids[1].0, 2);
    }

    #[test]
    fn resolve_display_id_finds_correct_uuid() {
        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();
        let todos = vec![
            (1, Todo {
                uuid: uuid_a,
                description: "a".to_owned(),
                due_date: date("2026-10-08"),
                completed_on: None,
                created_at: Utc::now(),
            }),
            (2, Todo {
                uuid: uuid_b,
                description: "b".to_owned(),
                due_date: date("2026-10-08"),
                completed_on: None,
                created_at: Utc::now(),
            }),
        ];
        assert_eq!(resolve_display_id(2, &todos), Some(uuid_b));
        assert_eq!(resolve_display_id(99, &todos), None);
    }

    #[test]
    fn overdue_open_todos_are_grouped_with_today() {
        let today = date("2026-08-11");
        let todo = Todo {
            uuid: Uuid::new_v4(),
            description: "overdue".to_owned(),
            due_date: date("2026-08-10"),
            completed_on: None,
            created_at: Utc::now(),
        };
        assert_eq!(group_name(&todo, today), "Today");
    }
}
