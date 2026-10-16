# todo desktop

A Tauri 2 desktop app for the `todo` CLI. It shares the exact same local data
as the CLI — `~/.todo/todos.db` and `~/.todo/config.toml` — so both apps work
on the same event log on the same machine.

## Build

Linux (Debian/Ubuntu) system dependencies:

```
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev patchelf
```

```bash
cargo build -p todo-desktop
```

For dev/bundling (`cargo tauri dev`, `cargo tauri build` to produce
installers), install the Tauri CLI:

```bash
cargo install tauri-cli --version ^2
```

## Architecture

- `desktop/ui/` — plain HTML/JS/CSS frontend, no Node build step. Talks to the
  Rust backend via Tauri's `invoke` API (`window.__TAURI__.core`).
- `desktop/src-tauri/src/commands.rs` — Tauri commands wrapping the shared
  `todo-core` crate (same code the CLI uses): list/add/edit/complete/remove
  todos, plus `sync_now`, `login`, `logout`, `auth_status`.
- Blocking core calls (sync, login loopback flow) run on
  `tauri::async_runtime::spawn_blocking` so the UI thread stays responsive.

## Shared store notes

- Display semantics match the CLI: default list hides completed todos
  (toggle "Show completed"), overdue items are highlighted.
- Login reuses the CLI's loopback-callback OIDC flow (opens the system
  browser); the session token is stored in the same `config.toml`.
