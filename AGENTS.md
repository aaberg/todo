# AGENTS.md — todo

A local-first todo CLI with multi-device sync via an event-sourced relay.

## Workspace Layout

```
todo/
├── Cargo.toml          # workspace root: members = [cli, common, relay]
├── cli/                # binary: todo (the CLI)
│   └── src/
│       ├── main.rs     # entry, command dispatch
│       ├── cli.rs      # clap: Cli, Commands
│       ├── db.rs       # local SQLite event store
│       ├── model.rs    # fold_events, display IDs, grouping/sorting
│       ├── event.rs    # domain: Event, EventType, EventPayload
│       ├── display.rs  # terminal output, colors, print_log
│       ├── date.rs     # relative date parsing (Nd syntax)
│       ├── config.rs   # ~/.todo/config.toml (sync_url, session_token, device_id)
│       └── sync.rs     # HTTP client: login, sync (push/pull), logout, whoami
├── common/             # library: todo-common
│   └── src/wire.rs     # WireEvent, Push/Pull/Me request+response types (HTTP contract)
└── relay/              # binary: todo-relay (the sync server)
    ├── .env.example    # required env vars template
    ├── Caddyfile       # TLS reverse proxy config
    ├── todo-relay.service  # systemd unit
    └── src/
        ├── main.rs     # axum bootstrap, public/protected route split
        ├── config.rs   # env var loading
        ├── db.rs       # SQLite: users, sessions, events tables
        ├── oidc.rs     # OIDC client: discovery, authorize URL, token exchange, validation
        ├── auth.rs     # AppState, PendingLogin, Bearer token middleware
        └── api.rs      # /auth/login, /auth/callback, /push, /pull, /me, /logout
```

## Architecture

**Event sourcing.** The CLI stores an append-only `events` table in local SQLite
(`~/.todo/todos.db`). There is no `todos` table. Current state is derived by folding
events: `fold_events()` in `cli/src/model.rs`. Events are sorted by `(timestamp, event_id)`
for deterministic order.

**Sync model.** The relay is a dumb event pipe. It stores events per user, dedupes by
`(user_id, event_id)`, and serves them back to other devices. Conflict resolution is
last-write-wins by event timestamp — convergence is guaranteed because all devices
fold the same event set in the same order.

**Wire format.** `common/src/wire.rs` defines the HTTP/JSON contract. The relay treats
`event_type` and `payload` as opaque strings/JSON — it never deserializes domain types.
The CLI converts between `Event` (typed) and `WireEvent` (opaque) in `cli/src/sync.rs`.

**Auth.** OIDC via Authelia. Flow: `todo login` → CLI starts loopback server → opens
browser to relay `/auth/login` → relay redirects to Authelia → user authenticates →
relay exchanges code, validates ID token (JWKS signature, issuer, audience, expiry,
nonce), checks `groups` claim against `TODO_ALLOWED_GROUPS` → relay creates session →
redirects browser to CLI loopback with session token → CLI saves token to
`~/.todo/config.toml` (mode 600).

**Display IDs are ephemeral.** Users see 1-based IDs (`todo done 3`). These are derived
from fold order + sort, reassigned on every invocation. The CLI resolves a display ID
to a todo UUID internally. UUIDs are the only stable identity.

## Build & Test

```bash
cargo build --workspace        # build all 3 crates
cargo build -p todo            # build CLI only
cargo build -p todo-relay      # build relay only
cargo test --workspace         # 36 tests
cargo build --release -p todo  # release CLI binary
```

## Run the CLI

```bash
cargo run -p todo -- <args>    # or use target/debug/todo
```

## Run the Relay (local)

```bash
TODO_RELAY_BIND=127.0.0.1:3000 \
TODO_RELAY_PUBLIC_URL=http://127.0.0.1:3000 \
TODO_OIDC_ISSUER=https://auth.aaberg.cc \
TODO_OIDC_CLIENT_ID=todo-relay \
TODO_OIDC_CLIENT_SECRET=<secret> \
TODO_RELAY_DATABASE=/tmp/relay.db \
cargo run -p todo-relay
```

See `relay/.env.example` for all env vars.

## Deploy the Relay (VPS)

```bash
cargo build --release -p todo-relay
sudo cp target/release/todo-relay /usr/local/bin/
sudo mkdir -p /var/lib/todo-relay /etc/todo-relay
sudo cp relay/.env.example /etc/todo-relay/env   # then edit with real values
sudo cp relay/todo-relay.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now todo-relay
```

Caddy (TLS) config is in `relay/Caddyfile`. Point `relay.yourdomain.com` at the VPS,
reverse proxy to `127.0.0.1:3000`.

## Key Conventions

- **No `todos` table.** Everything goes through `events`. The fold is the only way
  to read current state.
- **Relay is provider-agnostic and payload-agnostic.** Don't add domain logic to it.
- **Session tokens are SHA-256 hashed at rest.** Never log raw tokens.
- **`cli_callback` must be a loopback URL** (127.0.0.1, ::1, localhost). The relay
  validates this to prevent open redirect attacks.
- **Display IDs shift.** If a todo is added between two commands, IDs renumber.
  This is expected — same as `less`, `vim`, etc.
- **Prune is now soft-delete via events.** `todo prune` appends `Delete` events for
  all completed todos.

## Known Issues

- **Device ID mismatch:** The CLI has two device IDs — one in the local DB
  (`sync_state.device_id`, used when emitting events) and one in `config.toml`
  (`device_id`, used as `exclude_device` when pulling). They don't match, so every
  sync re-pulls the device's own events. Harmless (dedup by `event_id` catches them)
  but wasteful. Fix: use `config.device_id` as the single source.
- **No rate limiting on the relay.** Should add per-user limits.
- **No auto-sync.** Sync is manual (`todo sync`). Could add sync-on-write or a
  `todo sync --watch` daemon mode.
- **No relay-side event pruning.** The events table grows forever.

## Testing

```bash
cargo test --workspace
```

Test coverage includes:
- `cli/src/db.rs` — 6 convergence tests (two-machine sync simulation, LWW conflicts,
  delete-vs-update, dedup, complex interleaved scenario)
- `cli/src/model.rs` — fold_events unit tests (create, update, delete, resurrection,
  ordering, display IDs)
- `cli/src/sync.rs` — wire roundtrip tests, token extraction, URL encoding
- `cli/src/config.rs` — config serialization, auth state
