use serde::Serialize;
use todo_core::{
    config,
    db::Database,
    event::{EventPayload, EventType},
    model::{self, Todo},
    sync,
};
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoDto {
    pub uuid: String,
    pub description: String,
    pub due_date: String,
    pub completed_on: Option<String>,
}

fn database() -> Result<Database, String> {
    let directory = std::env::home_dir()
        .map(|h| h.join(".todo"))
        .ok_or("cannot determine home directory")?;
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    Database::open(directory.join("todos.db")).map_err(|e| e.to_string())
}

fn dto_list(db: &mut Database, all: bool) -> Result<Vec<TodoDto>, String> {
    let today = chrono::Local::now().date_naive();
    let events = db.all_events().map_err(|e| e.to_string())?;
    let todos = model::fold_events(&events);
    let display = model::prepare_for_display(todos, today, all);
    Ok(display
        .into_iter()
        .map(|(_, Todo { uuid, description, due_date, completed_on, .. })| TodoDto {
            uuid: uuid.to_string(),
            description,
            due_date: due_date.to_string(),
            completed_on: completed_on.map(|d| d.to_string()),
        })
        .collect())
}

#[tauri::command]
pub fn list_todos(all: bool) -> Result<Vec<TodoDto>, String> {
    let mut db = database()?;
    dto_list(&mut db, all)
}

#[tauri::command]
pub fn add_todo(description: String, due: Option<String>) -> Result<Vec<TodoDto>, String> {
    let today = chrono::Local::now().date_naive();
    let due_date = match due {
        Some(value) => todo_core::date::parse_relative_due_date(&value, today)
            .map_err(|e| e.to_string())?,
        None => today,
    };
    let mut db = database()?;
    db.emit(
        Uuid::new_v4(),
        EventType::Create,
        EventPayload::Create {
            description,
            due_date,
        },
    )
    .map_err(|e| e.to_string())?;
    dto_list(&mut db, false)
}

fn emit_update(
    db: &mut Database,
    uuid: &str,
    description: Option<String>,
    due_date: Option<chrono::NaiveDate>,
    completed_on: Option<Option<chrono::NaiveDate>>,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(uuid).map_err(|e| e.to_string())?;
    db.emit(
        uuid,
        EventType::Update,
        EventPayload::Update {
            description,
            due_date,
            completed_on,
        },
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_description(uuid: String, description: String) -> Result<Vec<TodoDto>, String> {
    let mut db = database()?;
    emit_update(&mut db, &uuid, Some(description), None, None)?;
    dto_list(&mut db, false)
}

#[tauri::command]
pub fn set_due_date(uuid: String, due: String) -> Result<Vec<TodoDto>, String> {
    let today = chrono::Local::now().date_naive();
    let mut db = database()?;
    let due_date =
        todo_core::date::parse_relative_due_date(&due, today).map_err(|e| e.to_string())?;
    emit_update(&mut db, &uuid, None, Some(due_date), None)?;
    dto_list(&mut db, false)
}

#[tauri::command]
pub fn set_completed(uuid: String, completed: bool) -> Result<Vec<TodoDto>, String> {
    let today = chrono::Local::now().date_naive();
    let mut db = database()?;
    let completed_on = Some(if completed { Some(today) } else { None });
    emit_update(&mut db, &uuid, None, None, completed_on)?;
    dto_list(&mut db, false)
}

#[tauri::command]
pub fn remove_todo(uuid: String) -> Result<Vec<TodoDto>, String> {
    let mut db = database()?;
    let uuid = Uuid::parse_str(&uuid).map_err(|e| e.to_string())?;
    db.emit(uuid, EventType::Delete, EventPayload::Delete)
        .map_err(|e| e.to_string())?;
    dto_list(&mut db, false)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncInfo {
    pub logged_in: bool,
    pub relay_url: Option<String>,
    pub email: Option<String>,
}

#[tauri::command]
pub fn auth_status() -> SyncInfo {
    let config = config::Config::load();
    let logged_in = config.sync_url.is_some() && config.session_token.is_some();
    SyncInfo {
        logged_in,
        relay_url: config.sync_url,
        email: None,
    }
}

#[tauri::command]
pub async fn sync_now() -> Result<(u64, u64), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let config = config::Config::load();
        let mut db = database()?;
        let stats = sync::sync(&mut db, &config).map_err(|e| e.to_string())?;
        Ok((stats.pulled as u64, stats.pushed as u64))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn login(relay_url: String) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut db = database()?;
        // The core login flow opens the system browser and runs a loopback
        // server; the relay redirects back with the session token.
        sync::login(&mut db, &relay_url).map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn logout() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut config = config::Config::load();
        sync::logout(&mut config).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
