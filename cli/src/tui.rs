use std::error::Error;
use std::io::{self, IsTerminal, Stdout};

use chrono::NaiveDate;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use uuid::Uuid;

use todo_core::db::Database;
use crate::display::print_list;
use todo_core::event::{EventPayload, EventType};
use todo_core::model::{fold_events, group_name, prepare_for_display, Todo};

const ORANGE: Color = Color::Rgb(255, 165, 0);
const RED: Color = Color::Red;
const GRAY: Color = Color::DarkGray;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Up,
    Down,
    Backspace,
    Other,
}

impl Key {
    fn from_event(event: &KeyEvent) -> Self {
        match event.code {
            KeyCode::Char(c) if event.modifiers == KeyModifiers::NONE || event.modifiers == KeyModifiers::SHIFT => Key::Char(c),
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Backspace => Key::Backspace,
            _ => Key::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Add,
    Edit(Uuid),
    ConfirmDelete(Uuid),
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Toggle(Uuid),
    Add(String),
    Edit(Uuid, String),
    Delete(Uuid),
    Quit,
}

#[derive(Debug)]
pub struct App {
    todos: Vec<(i64, Todo)>,
    today: NaiveDate,
    selected_index: usize,
    mode: Mode,
    input: String,
    message: String,
    quit: bool,
}

impl App {
    pub fn new(todos: Vec<(i64, Todo)>, today: NaiveDate) -> Self {
        Self {
            todos,
            today,
            selected_index: 0,
            mode: Mode::Normal,
            input: String::new(),
            message: String::new(),
            quit: false,
        }
    }

    fn selected_uuid(&self) -> Option<Uuid> {
        self.todos.get(self.selected_index).map(|(_, t)| t.uuid)
    }

    fn select_uuid(&mut self, uuid: Uuid) {
        if let Some(index) = self.todos.iter().position(|(_, t)| t.uuid == uuid) {
            self.selected_index = index;
        } else if !self.todos.is_empty() {
            self.selected_index = self.selected_index.min(self.todos.len() - 1);
        } else {
            self.selected_index = 0;
        }
    }

    fn reset_selection(&mut self) {
        self.selected_index = 0;
    }

    fn handle_key(&mut self, key: Key) -> Action {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Add => self.handle_input_key(key),
            Mode::Edit(uuid) => {
                let action = self.handle_input_key(key);
                if let Action::Add(text) = action {
                    Action::Edit(uuid, text)
                } else {
                    action
                }
            }
            Mode::ConfirmDelete(uuid) => self.handle_confirm_key(key, uuid),
        }
    }

    fn handle_normal_key(&mut self, key: Key) -> Action {
        match key {
            Key::Up => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                Action::None
            }
            Key::Down => {
                if self.selected_index + 1 < self.todos.len() {
                    self.selected_index += 1;
                }
                Action::None
            }
            Key::Char(' ') | Key::Enter => match self.selected_uuid() {
                Some(uuid) => Action::Toggle(uuid),
                None => Action::None,
            },
            Key::Char('a') => {
                self.mode = Mode::Add;
                self.input.clear();
                Action::None
            }
            Key::Char('e') => match self.selected_uuid() {
                Some(uuid) => {
                    let description = self
                        .todos
                        .iter()
                        .find(|(_, t)| t.uuid == uuid)
                        .map(|(_, t)| t.description.clone())
                        .unwrap_or_default();
                    self.mode = Mode::Edit(uuid);
                    self.input = description;
                    Action::None
                }
                None => Action::None,
            },
            Key::Char('x') => match self.selected_uuid() {
                Some(uuid) => {
                    self.mode = Mode::ConfirmDelete(uuid);
                    Action::None
                }
                None => Action::None,
            },
            Key::Char('q') | Key::Esc => {
                self.quit = true;
                Action::Quit
            }
            _ => Action::None,
        }
    }

    fn handle_input_key(&mut self, key: Key) -> Action {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                Action::None
            }
            Key::Enter => {
                let text = self.input.trim().to_string();
                self.mode = Mode::Normal;
                self.input.clear();
                if text.is_empty() {
                    self.message = "Nothing entered.".to_string();
                    Action::None
                } else {
                    Action::Add(text)
                }
            }
            Key::Backspace => {
                self.input.pop();
                Action::None
            }
            Key::Char(c) => {
                self.input.push(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_confirm_key(&mut self, key: Key, uuid: Uuid) -> Action {
        match key {
            Key::Char('y') | Key::Char('Y') | Key::Enter => {
                self.mode = Mode::Normal;
                Action::Delete(uuid)
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                self.mode = Mode::Normal;
                Action::None
            }
            _ => Action::None,
        }
    }
}

fn apply_action(db: &mut Database, action: &Action, today: NaiveDate) -> Result<(), Box<dyn Error>> {
    match action {
        Action::None | Action::Quit => {}
        Action::Toggle(uuid) => {
            let todos = fold_events(&db.all_events()?);
            let todo = todos.iter().find(|t| t.uuid == *uuid);
            let Some(todo) = todo else {
                return Ok(());
            };
            let completed_on = if todo.completed_on.is_some() { None } else { Some(today) };
            db.emit(
                *uuid,
                EventType::Update,
                EventPayload::Update {
                    description: None,
                    due_date: None,
                    completed_on: Some(completed_on),
                },
            )?;
        }
        Action::Add(description) => {
            db.emit(
                Uuid::new_v4(),
                EventType::Create,
                EventPayload::Create {
                    description: description.clone(),
                    due_date: today,
                },
            )?;
        }
        Action::Edit(uuid, description) => {
            db.emit(
                *uuid,
                EventType::Update,
                EventPayload::Update {
                    description: Some(description.clone()),
                    due_date: None,
                    completed_on: None,
                },
            )?;
        }
        Action::Delete(uuid) => {
            db.emit(*uuid, EventType::Delete, EventPayload::Delete)?;
        }
    }
    Ok(())
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self, Box<dyn Error>> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}

pub fn run(db: &mut Database, today: NaiveDate) -> Result<(), Box<dyn Error>> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err("interactive mode requires a terminal".into());
    }
    let todos = fold_events(&db.all_events()?);
    let display = prepare_for_display(todos, today, false);
    let mut app = App::new(display, today);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        previous_hook(info);
    }));

    let result = event_loop(&mut app, db, today);

    result?;

    let todos = fold_events(&db.all_events()?);
    let display = prepare_for_display(todos, today, false);
    print_list(&display, today);
    Ok(())
}

fn event_loop(
    app: &mut App,
    db: &mut Database,
    today: NaiveDate,
) -> Result<(), Box<dyn Error>> {
    let mut guard = TerminalGuard::new()?;
    let mut list_state = ListState::default();

    loop {
        let selected_uuid = app.selected_uuid();
        list_state.select(selected_item_index(app));

        let terminal = &mut guard.terminal;
        terminal.draw(|frame| {
            let area = frame.area();
            if matches!(app.mode, Mode::ConfirmDelete(_)) {
                render_confirm(frame, area, app);
            } else {
                render_list(frame, area, app, &mut list_state);
            }
        })?;

        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        let action = app.handle_key(Key::from_event(&key));

        if action == Action::Quit {
            return Ok(());
        }

        apply_action(db, &action, today)?;

        if action != Action::None {
            let todos = fold_events(&db.all_events()?);
            app.todos = prepare_for_display(todos, today, false);
            match (&action, selected_uuid) {
                (Action::Add(_), _) => {
                    app.message = "Added.".to_string();
                    app.reset_selection();
                }
                (Action::Delete(_), Some(uuid)) => {
                    app.message = "Deleted.".to_string();
                    app.select_uuid(uuid);
                }
                (Action::Delete(_), None) => {
                    app.message = "Deleted.".to_string();
                    app.reset_selection();
                }
                (_, Some(uuid)) => app.select_uuid(uuid),
                (_, None) => app.reset_selection(),
            }
        }
    }
}

fn selected_item_index(app: &App) -> Option<usize> {
    if app.todos.is_empty() {
        return None;
    }
    let mut item_index = 0;
    let mut current_group: Option<String> = None;
    for (index, (_, todo)) in app.todos.iter().enumerate() {
        let group = group_name(todo, app.today);
        if current_group.as_deref() != Some(group.as_str()) {
            item_index += 1;
            current_group = Some(group);
        }
        if index == app.selected_index {
            return Some(item_index);
        }
        item_index += 1;
    }
    None
}

fn render_list(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &App,
    list_state: &mut ListState,
) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    if app.todos.is_empty() {
        let empty = Paragraph::new("No todos. Press 'a' to add one.");
        frame.render_widget(empty, chunks[0]);
    } else {
        let mut items: Vec<ratatui::widgets::ListItem> = Vec::new();
        let mut current_group: Option<String> = None;
        for (index, (id, todo)) in app.todos.iter().enumerate() {
            let group = group_name(todo, app.today);
            if current_group.as_deref() != Some(group.as_str()) {
                let heading_style = if group == "Overdue" {
                    Style::default().fg(RED).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(ORANGE)
                };
                items.push(ratatui::widgets::ListItem::new(Line::styled(
                    group.clone(),
                    heading_style,
                )));
                current_group = Some(group);
            }
            let selected = index == app.selected_index;
            let base_style = todo_style(todo, app.today);
            let state = if todo.completed_on.is_some() { "x" } else { " " };
            let marker = if selected { ">" } else { " " };
            let prefix = Span::styled(
                format!("{marker} [{state}] {id} "),
                if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    base_style
                },
            );
            let text = Span::styled(
                todo.description.clone(),
                if selected {
                    Style::default().add_modifier(Modifier::REVERSED).patch(base_style)
                } else {
                    base_style
                },
            );
            let line = Line::from(vec![prefix, text]);
            items.push(ratatui::widgets::ListItem::new(line));
        }
        let list = List::new(items);
        frame.render_stateful_widget(list, chunks[0], list_state);
    }

    let footer_text = match &app.mode {
        Mode::Add => Line::from(vec![
            Span::raw("Add: "),
            Span::styled(app.input.clone(), Style::default().fg(ORANGE)),
            Span::raw("_"),
        ]),
        Mode::Edit(_) => Line::from(vec![
            Span::raw("Edit: "),
            Span::styled(app.input.clone(), Style::default().fg(ORANGE)),
            Span::raw("_"),
        ]),
        _ => Line::from(Span::styled(
            "↑/↓ select · space/enter toggle · a add · e edit · x delete · q quit",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(footer_text), chunks[1]);

    if !app.message.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                app.message.clone(),
                Style::default().fg(Color::DarkGray),
            )),
            chunks[2],
        );
    }
}

fn confirm_text(description: &str, max_width: usize) -> String {
    let full = format!("Delete \"{description}\" permanently? (y/n)");
    if full.chars().count() <= max_width {
        return full;
    }
    let prefix = "Delete \"";
    let suffix = "… \" permanently? (y/n)";
    let keep = max_width.saturating_sub(prefix.chars().count() + suffix.chars().count());
    if keep > 0 {
        let trimmed: String = description.chars().take(keep).collect();
        return format!("{prefix}{trimmed}{suffix}");
    }
    let fallback = "Delete permanently? (y/n)";
    if fallback.chars().count() <= max_width {
        return fallback.to_string();
    }
    "Delete? (y/n)".to_string()
}

fn render_confirm(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let Mode::ConfirmDelete(uuid) = &app.mode else {
        return;
    };
    let description = app
        .todos
        .iter()
        .find(|(_, t)| t.uuid == *uuid)
        .map(|(_, t)| t.description.as_str())
        .unwrap_or("");
    let text = confirm_text(description, area.width.saturating_sub(4) as usize);

    let box_width = (text.chars().count().max(20) as u16 + 4).min(area.width);
    let box_height = 3u16;
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let popup = ratatui::layout::Rect {
        x,
        y,
        width: box_width.min(area.width),
        height: box_height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(RED));
    let paragraph = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(RED));
    frame.render_widget(paragraph, popup);
}

fn todo_style(todo: &Todo, today: NaiveDate) -> Style {
    if todo.completed_on.is_some() {
        Style::default().fg(GRAY).add_modifier(Modifier::CROSSED_OUT)
    } else if todo.due_date < today {
        Style::default().fg(RED)
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(uuid: Uuid, description: &str, due_date: NaiveDate, completed: bool) -> Todo {
        Todo {
            uuid,
            description: description.to_string(),
            due_date,
            completed_on: completed.then_some(due_date),
            created_at: chrono::Utc::now(),
        }
    }

    fn sample_app() -> App {
        let today = NaiveDate::from_ymd_opt(2024, 1, 10).unwrap();
        let todos = vec![
            (1, todo(Uuid::new_v4(), "first", today, false)),
            (2, todo(Uuid::new_v4(), "second", today, true)),
        ];
        App::new(todos, today)
    }

    #[test]
    fn moving_down_and_up() {
        let mut app = sample_app();
        assert_eq!(app.selected_index, 0);
        app.handle_key(Key::Down);
        assert_eq!(app.selected_index, 1);
        app.handle_key(Key::Down);
        assert_eq!(app.selected_index, 1);
        app.handle_key(Key::Up);
        assert_eq!(app.selected_index, 0);
        app.handle_key(Key::Up);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn space_toggles_selected_todo() {
        let mut app = sample_app();
        let uuid = app.todos[0].1.uuid;
        assert_eq!(app.handle_key(Key::Char(' ')), Action::Toggle(uuid));
    }

    #[test]
    fn enter_toggles_selected_todo() {
        let mut app = sample_app();
        let uuid = app.todos[0].1.uuid;
        assert_eq!(app.handle_key(Key::Enter), Action::Toggle(uuid));
    }

    #[test]
    fn add_mode_collects_input_and_confirms() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        for c in "buy milk".chars() {
            app.handle_key(Key::Char(c));
        }
        assert_eq!(app.input, "buy milk");
        assert_eq!(app.handle_key(Key::Enter), Action::Add("buy milk".to_string()));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn add_mode_backspace_and_cancel() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        app.handle_key(Key::Char('a'));
        app.handle_key(Key::Char('b'));
        app.handle_key(Key::Backspace);
        assert_eq!(app.input, "a");
        app.handle_key(Key::Esc);
        assert_eq!(app.mode, Mode::Normal);
        let uuid = app.todos[0].1.uuid;
        assert_eq!(app.handle_key(Key::Enter), Action::Toggle(uuid));
    }

    #[test]
    fn empty_input_adds_nothing() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        assert_eq!(app.handle_key(Key::Enter), Action::None);
    }

    #[test]
    fn edit_mode_prefills_and_edits_selected() {
        let mut app = sample_app();
        let uuid = app.todos[0].1.uuid;
        app.handle_key(Key::Char('e'));
        assert_eq!(app.mode, Mode::Edit(uuid));
        assert_eq!(app.input, "first");
        for _ in 0..5 {
            app.handle_key(Key::Backspace);
        }
        for c in "third".chars() {
            app.handle_key(Key::Char(c));
        }
        assert_eq!(app.handle_key(Key::Enter), Action::Edit(uuid, "third".to_string()));
    }

    #[test]
    fn delete_requires_confirmation() {
        let mut app = sample_app();
        let uuid = app.todos[0].1.uuid;
        app.handle_key(Key::Char('x'));
        assert_eq!(app.mode, Mode::ConfirmDelete(uuid));
        assert_eq!(app.handle_key(Key::Char('n')), Action::None);
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(Key::Char('x'));
        assert_eq!(app.handle_key(Key::Enter), Action::Delete(uuid));
    }

    #[test]
    fn delete_confirmation_rejects_other_keys() {
        let mut app = sample_app();
        let uuid = app.todos[0].1.uuid;
        app.handle_key(Key::Char('x'));
        assert_eq!(app.handle_key(Key::Char('z')), Action::None);
        assert_eq!(app.mode, Mode::ConfirmDelete(uuid));
    }

    #[test]
    fn quit_key_quits() {
        let mut app = sample_app();
        assert_eq!(app.handle_key(Key::Char('q')), Action::Quit);
        assert!(app.quit);
    }

    #[test]
    fn selected_item_index_counts_group_headings() {
        let today = NaiveDate::from_ymd_opt(2024, 1, 10).unwrap();
        let yesterday = today.pred_opt().unwrap();
        let overdue = todo(Uuid::new_v4(), "overdue", yesterday, false);
        let completed_overdue = todo(Uuid::new_v4(), "done overdue", yesterday, true);
        let current = todo(Uuid::new_v4(), "current", today, false);
        let todos = vec![(1, overdue), (2, completed_overdue), (3, current)];
        let mut app = App::new(todos, today);
        assert_eq!(selected_item_index(&app), Some(1));
        app.selected_index = 1;
        assert_eq!(selected_item_index(&app), Some(2));
        app.selected_index = 2;
        assert_eq!(selected_item_index(&app), Some(4));
    }

    #[test]
    fn selected_item_index_empty_returns_none() {
        let today = NaiveDate::from_ymd_opt(2024, 1, 10).unwrap();
        let app = App::new(Vec::new(), today);
        assert_eq!(selected_item_index(&app), None);
    }

    #[test]
    fn confirm_text_fits_within_budget() {
        let description = "d".repeat(200);
        let text = confirm_text(&description, 40);
        assert!(text.chars().count() <= 40, "text was {} chars", text.chars().count());
        assert!(text.starts_with("Delete \""));
        assert!(text.ends_with("… \" permanently? (y/n)"));
    }

    #[test]
    fn confirm_text_short_description_untouched() {
        let text = confirm_text("milk", 80);
        assert_eq!(text, "Delete \"milk\" permanently? (y/n)");
    }

    #[test]
    fn confirm_text_narrow_terminal_falls_back() {
        let description = "d".repeat(200);
        let text = confirm_text(&description, 30);
        assert!(text.chars().count() <= 30);
        assert!(text.contains("(y/n)"));
        let text = confirm_text(&description, 12);
        assert_eq!(text, "Delete? (y/n)");
    }

    #[test]
    fn selection_restored_by_uuid() {
        let mut app = sample_app();
        let second = app.todos[1].1.uuid;
        app.select_uuid(second);
        assert_eq!(app.selected_index, 1);
        app.todos.remove(0);
        app.select_uuid(second);
        assert_eq!(app.selected_index, 0);
        app.select_uuid(Uuid::new_v4());
        assert_eq!(app.selected_index, 0);
    }
}
