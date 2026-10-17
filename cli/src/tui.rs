use std::error::Error;
use std::io::{self, IsTerminal, Stdout};

use chrono::{Datelike, NaiveDate};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use uuid::Uuid;

use crate::db::Database;
use crate::date::{format_date, parse_relative_due_date};
use crate::display::print_list;
use crate::event::{EventPayload, EventType};
use crate::model::{fold_events, group_name, prepare_for_display, Todo};

const ORANGE: Color = Color::Rgb(255, 165, 0);
const RED: Color = Color::Red;
const GRAY: Color = Color::DarkGray;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
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
            KeyCode::Tab => Key::Tab,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Backspace => Key::Backspace,
            _ => Key::Other,
        }
    }
}

/// One of the due-date suggestions shown in the picker, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DueChoice {
    Today,
    Tomorrow,
    InTwoDays,
    NextMonday,
    InOneWeek,
    Other,
}

impl DueChoice {
    const ALL: [DueChoice; 6] = [
        DueChoice::Today,
        DueChoice::Tomorrow,
        DueChoice::InTwoDays,
        DueChoice::NextMonday,
        DueChoice::InOneWeek,
        DueChoice::Other,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    fn label(self) -> &'static str {
        match self {
            DueChoice::Today => "Today",
            DueChoice::Tomorrow => "Tomorrow",
            DueChoice::InTwoDays => "In two days",
            DueChoice::NextMonday => "On Monday",
            DueChoice::InOneWeek => "In one week",
            DueChoice::Other => "Other…",
        }
    }

    /// Resolve to a concrete due date. Returns Err when the date arithmetic
    /// would overflow NaiveDate; the caller keeps the picker open and shows
    /// the error.
    fn resolve(self, today: NaiveDate) -> Result<NaiveDate, Box<dyn Error>> {
        let days = match self {
            DueChoice::Today => 0,
            DueChoice::Tomorrow => 1,
            DueChoice::InTwoDays => 2,
            DueChoice::NextMonday => 7 - today.weekday().num_days_from_monday() as i64,
            DueChoice::InOneWeek => 7,
            DueChoice::Other => return Err("choose a relative date like 1d or 7d".into()),
        };
        today
            .checked_add_signed(chrono::TimeDelta::days(days))
            .ok_or_else(|| "due date is out of range".into())
    }

    /// The gray secondary text shown on the right of each picker row.
    fn hint(self, today: NaiveDate) -> String {
        match self {
            DueChoice::Other => "enter 1d, 7d, …".to_string(),
            choice => match choice.resolve(today) {
                Ok(date) => format_date(date),
                Err(_) => "out of range".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    AddDescription,
    ChooseDue,
    CustomDue,
    Edit(Uuid),
    ConfirmDelete(Uuid),
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Toggle(Uuid),
    Add { description: String, due_date: NaiveDate },
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
    /// Description captured when leaving AddDescription via Enter; restored when
    /// returning from the picker. None in every other mode.
    pending_description: Option<String>,
    due_choice: DueChoice,
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
            pending_description: None,
            due_choice: DueChoice::Today,
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
            Mode::AddDescription => self.handle_add_description_key(key),
            Mode::ChooseDue => self.handle_choose_due_key(key),
            Mode::CustomDue => self.handle_custom_due_key(key),
            Mode::Edit(uuid) => {
                let action = self.handle_edit_input_key(key);
                if let Action::Add { description, .. } = action {
                    Action::Edit(uuid, description)
                } else {
                    action
                }
            }
            Mode::ConfirmDelete(uuid) => self.handle_confirm_key(key, uuid),
        }
    }

    fn clear_add_state(&mut self) {
        self.input.clear();
        self.pending_description = None;
        self.due_choice = DueChoice::Today;
    }

    /// Shared text editing for the add-description and edit modes. `Enter`
    /// produces `Action::Add` carrying today's date; the caller decides what
    /// to do with it (confirm the add, or rewrite it as an edit).
    fn handle_text_input_key(&mut self, key: Key) -> Action {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                self.clear_add_state();
                Action::None
            }
            Key::Enter => {
                let text = self.input.trim().to_string();
                self.mode = Mode::Normal;
                self.clear_add_state();
                if text.is_empty() {
                    self.message = "Nothing entered.".to_string();
                    Action::None
                } else {
                    Action::Add {
                        description: text,
                        due_date: self.today,
                    }
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

    fn handle_add_description_key(&mut self, key: Key) -> Action {
        match key {
            Key::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    self.message = "Nothing entered.".to_string();
                    Action::None
                } else {
                    self.pending_description = Some(text);
                    self.input.clear();
                    self.due_choice = DueChoice::Today;
                    self.mode = Mode::ChooseDue;
                    Action::None
                }
            }
            _ => self.handle_text_input_key(key),
        }
    }

    fn handle_edit_input_key(&mut self, key: Key) -> Action {
        match key {
            // Tab has no meaning while editing; ignore it.
            Key::Tab => Action::None,
            _ => self.handle_text_input_key(key),
        }
    }

    fn handle_choose_due_key(&mut self, key: Key) -> Action {
        match key {
            Key::Up => {
                self.due_choice = DueChoice::ALL[self.due_choice.index().saturating_sub(1)];
                Action::None
            }
            Key::Down => {
                let next = (self.due_choice.index() + 1).min(DueChoice::ALL.len() - 1);
                self.due_choice = DueChoice::ALL[next];
                Action::None
            }
            Key::Enter => match self.due_choice {
                DueChoice::Other => {
                    self.input.clear();
                    self.mode = Mode::CustomDue;
                    Action::None
                }
                choice => match choice.resolve(self.today) {
                    Ok(due_date) => {
                        let Some(description) = self.pending_description.take() else {
                            self.mode = Mode::Normal;
                            self.clear_add_state();
                            return Action::None;
                        };
                        self.mode = Mode::Normal;
                        self.clear_add_state();
                        Action::Add {
                            description,
                            due_date,
                        }
                    }
                    Err(error) => {
                        self.message = error.to_string();
                        Action::None
                    }
                },
            },
            Key::Esc => {
                self.mode = Mode::AddDescription;
                self.input = self.pending_description.take().unwrap_or_default();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_custom_due_key(&mut self, key: Key) -> Action {
        match key {
            Key::Esc => {
                self.mode = Mode::ChooseDue;
                self.due_choice = DueChoice::Other;
                self.input.clear();
                Action::None
            }
            Key::Enter => {
                let text = self.input.trim().to_string();
                match parse_relative_due_date(&text, self.today) {
                    Ok(due_date) => {
                        let Some(description) = self.pending_description.take() else {
                            self.mode = Mode::Normal;
                            self.clear_add_state();
                            return Action::None;
                        };
                        self.mode = Mode::Normal;
                        self.clear_add_state();
                        Action::Add {
                            description,
                            due_date,
                        }
                    }
                    Err(error) => {
                        self.message = error.to_string();
                        Action::None
                    }
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
                self.mode = Mode::AddDescription;
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
        Action::Add { description, due_date } => {
            db.emit(
                Uuid::new_v4(),
                EventType::Create,
                EventPayload::Create {
                    description: description.clone(),
                    due_date: *due_date,
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
                if matches!(app.mode, Mode::AddDescription) {
                    render_add_input(frame, area, app);
                }
                if matches!(app.mode, Mode::ChooseDue) {
                    render_due_picker(frame, area, app);
                }
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
                (Action::Add { .. }, _) => {
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
        Mode::AddDescription => Line::from(Span::styled(
            "Enter choose due date · Esc cancel",
            Style::default().fg(GRAY),
        )),
        Mode::ChooseDue => Line::from(Span::styled(
            "↑/↓ select · Enter confirm · Esc back",
            Style::default().fg(GRAY),
        )),
        Mode::CustomDue => Line::from(vec![
            Span::raw("Due: "),
            Span::styled(app.input.clone(), Style::default().fg(ORANGE)),
            Span::raw("_  "),
            Span::styled("Examples: 1d, 7d · Esc back", Style::default().fg(GRAY)),
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

fn render_add_input(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let input_display = if app.input.is_empty() {
        " _ ".to_string()
    } else {
        format!(" {}_ ", app.input)
    };
    let content_width = input_display.chars().count().max(20);
    let box_width = ((content_width + 4) as u16).min(area.width).max(10);
    let box_height = 5u16.min(area.height).max(3);
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let popup = ratatui::layout::Rect {
        x,
        y,
        width: box_width,
        height: box_height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("New todo");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Vertically center the input text within the inner area.
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let paragraph = Paragraph::new(Line::from(Span::styled(
        input_display,
        Style::default().fg(ORANGE),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(paragraph, chunks[1]);
}

fn render_due_picker(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    // Fixed-width label column so the date hints align vertically.
    let label_width = DueChoice::ALL
        .iter()
        .map(|c| c.label().chars().count())
        .max()
        .unwrap_or(0);
    let rows: Vec<Line> = DueChoice::ALL
        .iter()
        .map(|choice| {
            let selected = *choice == app.due_choice;
            let marker = if selected { ">" } else { " " };
            let label_style = if selected {
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let hint_style = Style::default().fg(GRAY);
            let label = choice.label();
            let padding = label_width - label.chars().count();
            let gap = "    ";
            Line::from(vec![
                Span::raw(format!("{marker} ")),
                Span::styled(label.to_string(), label_style),
                Span::raw(format!("{}{}", " ".repeat(padding), gap)),
                Span::styled(choice.hint(app.today), hint_style),
            ])
        })
        .collect();

    let content_width = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let box_width = ((content_width + 4) as u16).min(area.width).max(1);
    let box_height = (rows.len() as u16 + 2).min(area.height).max(1);
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let popup = ratatui::layout::Rect {
        x,
        y,
        width: box_width,
        height: box_height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Due date");
    let paragraph = Paragraph::new(rows).block(block);
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
    fn add_mode_collects_input_and_opens_due_picker_on_enter() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        for c in "buy milk".chars() {
            app.handle_key(Key::Char(c));
        }
        assert_eq!(app.input, "buy milk");
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::ChooseDue);
        assert_eq!(app.pending_description.as_deref(), Some("buy milk"));
        assert!(app.input.is_empty());
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

    fn app_entering_add(today: NaiveDate) -> App {
        let mut app = App::new(Vec::new(), today);
        app.handle_key(Key::Char('a'));
        app
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(Key::Char(c));
        }
    }

    #[test]
    fn enter_after_description_opens_picker_defaulting_to_today() {
        let mut app = sample_app();
        let today = app.today;
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "buy milk");
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::ChooseDue);
        assert_eq!(app.due_choice, DueChoice::Today);
        assert_eq!(
            app.handle_key(Key::Enter),
            Action::Add {
                description: "buy milk".to_string(),
                due_date: today,
            }
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn enter_opens_picker_and_preserves_description() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "  buy milk  ");
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::ChooseDue);
        assert_eq!(app.pending_description.as_deref(), Some("buy milk"));
        assert!(app.input.is_empty());
    }

    #[test]
    fn enter_with_empty_description_stays_in_add() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::AddDescription);
        assert_eq!(app.pending_description, None);
        assert_eq!(app.message, "Nothing entered.");

        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "   ");
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::AddDescription);
    }

    #[test]
    fn picker_defaults_to_today_and_navigation_is_bounded() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "buy milk");
        app.handle_key(Key::Enter);
        assert_eq!(app.due_choice, DueChoice::Today);

        app.handle_key(Key::Up);
        assert_eq!(app.due_choice, DueChoice::Today);

        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::Tomorrow);
        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::InTwoDays);
        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::NextMonday);
        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::InOneWeek);
        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::Other);
        app.handle_key(Key::Down);
        assert_eq!(app.due_choice, DueChoice::Other);

        app.handle_key(Key::Up);
        assert_eq!(app.due_choice, DueChoice::InOneWeek);
    }

    #[test]
    fn concrete_choices_resolve_to_offsets_0_1_2() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        assert_eq!(DueChoice::Today.resolve(today).unwrap(), today);
        assert_eq!(
            DueChoice::Tomorrow.resolve(today).unwrap(),
            today + chrono::TimeDelta::days(1)
        );
        assert_eq!(
            DueChoice::InTwoDays.resolve(today).unwrap(),
            today + chrono::TimeDelta::days(2)
        );
        assert_eq!(
            DueChoice::InOneWeek.resolve(today).unwrap(),
            today + chrono::TimeDelta::days(7)
        );
    }

    #[test]
    fn next_monday_is_strictly_in_the_future() {
        // 2026-10-17 is a Saturday, 2026-10-18 a Sunday, 2026-10-19 a Monday.
        let saturday = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let sunday = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let tuesday = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();

        assert_eq!(
            DueChoice::NextMonday.resolve(saturday).unwrap(),
            monday,
            "Saturday -> Monday two days later"
        );
        assert_eq!(
            DueChoice::NextMonday.resolve(sunday).unwrap(),
            monday,
            "Sunday -> Monday one day later"
        );
        assert_eq!(
            DueChoice::NextMonday.resolve(monday).unwrap(),
            monday + chrono::TimeDelta::days(7),
            "Monday -> next Monday seven days later"
        );
        assert_eq!(
            DueChoice::NextMonday.resolve(tuesday).unwrap(),
            monday + chrono::TimeDelta::days(7),
            "Tuesday -> next Monday"
        );
    }

    #[test]
    fn duplicate_dates_keep_both_choices() {
        // Saturday: In two days and On Monday both resolve to Monday.
        let saturday = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        assert_eq!(
            DueChoice::InTwoDays.resolve(saturday).unwrap(),
            DueChoice::NextMonday.resolve(saturday).unwrap()
        );
        assert_eq!(DueChoice::ALL.len(), 6);
    }

    #[test]
    fn picker_enter_adds_with_selected_due_date() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let mut app = app_entering_add(today);
        type_text(&mut app, "buy milk");
        app.handle_key(Key::Enter);

        app.handle_key(Key::Down);
        assert_eq!(
            app.handle_key(Key::Enter),
            Action::Add {
                description: "buy milk".to_string(),
                due_date: today + chrono::TimeDelta::days(1),
            }
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.pending_description, None);
    }

    #[test]
    fn esc_from_picker_restores_description_input() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "buy milk");
        app.handle_key(Key::Enter);
        app.handle_key(Key::Down);
        assert_eq!(app.handle_key(Key::Esc), Action::None);
        assert_eq!(app.mode, Mode::AddDescription);
        assert_eq!(app.input, "buy milk");
        assert_eq!(app.pending_description, None);
    }

    #[test]
    fn other_opens_custom_input() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "buy milk");
        app.handle_key(Key::Enter);
        for _ in 0..5 {
            app.handle_key(Key::Down);
        }
        assert_eq!(app.due_choice, DueChoice::Other);
        assert_eq!(app.handle_key(Key::Enter), Action::None);
        assert_eq!(app.mode, Mode::CustomDue);
        assert_eq!(app.pending_description.as_deref(), Some("buy milk"));
    }

    #[test]
    fn custom_input_valid_values_add_with_expected_due_date() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        for (input, offset) in [("1d", 1), ("7d", 7)] {
            let mut app = app_entering_add(today);
            type_text(&mut app, "buy milk");
            app.handle_key(Key::Enter);
            for _ in 0..5 {
                app.handle_key(Key::Down);
            }
            app.handle_key(Key::Enter);
            type_text(&mut app, input);
            assert_eq!(
                app.handle_key(Key::Enter),
                Action::Add {
                    description: "buy milk".to_string(),
                    due_date: today + chrono::TimeDelta::days(offset),
                }
            );
            assert_eq!(app.mode, Mode::Normal);
        }
    }

    #[test]
    fn custom_input_invalid_values_stay_and_show_error() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        for input in ["tomorrow", "-1d", ""] {
            let mut app = app_entering_add(today);
            type_text(&mut app, "buy milk");
            app.handle_key(Key::Enter);
            for _ in 0..5 {
                app.handle_key(Key::Down);
            }
            app.handle_key(Key::Enter);
            type_text(&mut app, input);
            assert_eq!(app.handle_key(Key::Enter), Action::None);
            assert_eq!(app.mode, Mode::CustomDue);
            assert_eq!(app.pending_description.as_deref(), Some("buy milk"));
            assert_eq!(app.input, input);
            assert!(!app.message.is_empty());
        }
    }

    #[test]
    fn esc_from_custom_input_returns_to_picker_with_other_selected() {
        let mut app = sample_app();
        app.handle_key(Key::Char('a'));
        type_text(&mut app, "buy milk");
        app.handle_key(Key::Enter);
        for _ in 0..5 {
            app.handle_key(Key::Down);
        }
        app.handle_key(Key::Enter);
        type_text(&mut app, "7d");
        assert_eq!(app.handle_key(Key::Esc), Action::None);
        assert_eq!(app.mode, Mode::ChooseDue);
        assert_eq!(app.due_choice, DueChoice::Other);
        assert_eq!(app.pending_description.as_deref(), Some("buy milk"));
    }

    #[test]
    fn applying_add_action_persists_selected_due_date() {
        let mut db = Database::open_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let due_date = today + chrono::TimeDelta::days(7);
        apply_action(
            &mut db,
            &Action::Add {
                description: "buy milk".to_string(),
                due_date,
            },
            today,
        )
        .unwrap();
        let todos = fold_events(&db.all_events().unwrap());
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].description, "buy milk");
        assert_eq!(todos[0].due_date, due_date);
    }

    #[test]
    fn picker_hints_show_resolved_dates() {
        let saturday = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        assert_eq!(DueChoice::Today.hint(saturday), "Oct 17th, 2026");
        assert_eq!(DueChoice::Tomorrow.hint(saturday), "Oct 18th, 2026");
        assert_eq!(DueChoice::InTwoDays.hint(saturday), "Oct 19th, 2026");
        assert_eq!(DueChoice::NextMonday.hint(saturday), "Oct 19th, 2026");
        assert_eq!(DueChoice::InOneWeek.hint(saturday), "Oct 24th, 2026");
        assert_eq!(DueChoice::Other.hint(saturday), "enter 1d, 7d, …");
    }
}
