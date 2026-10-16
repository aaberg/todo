mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::list_todos,
            commands::add_todo,
            commands::set_description,
            commands::set_due_date,
            commands::set_completed,
            commands::remove_todo,
            commands::auth_status,
            commands::sync_now,
            commands::login,
            commands::logout,
        ])
        .run(tauri::generate_context!())
        .expect("error while running todo desktop app");
}
