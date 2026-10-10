# todo

A small, local-first todo list with multi-device sync.

Todos live in a local SQLite database. An append-only event log is the source of
truth — current state is derived by folding events. A lightweight relay server
syncs the event log across machines. Last-write-wins by timestamp; all devices
converge to the same state.

## Quick start

### CLI

```bash
cargo build --release -p todo
./target/release/todo --help
```

```
todo add "Buy milk" 1d      # add a todo due tomorrow
todo list                   # show todos grouped by day
todo list --all             # include completed todos from every day
todo done 1                 # mark todo 1 as done
todo undone 1               # mark todo 1 as not done
todo edit 1 "Buy oat milk"  # change description
todo remove 1               # permanently delete
todo log 1                  # show event history for todo 1
todo prune                  # delete all completed todos
```

### Sync between machines

```bash
# 1. Deploy the relay (see "Deploy the relay" below), then on each machine:
todo login --relay https://relay.yourdomain.com   # opens browser, log in via Authelia
todo whoami                                       # confirm: Logged in as you@example.com
todo sync                                         # push local events, pull remote events
todo logout                                       # revoke session
```

### Run the relay locally (development)

```bash
cp relay/.env.example /tmp/todo-relay.env
# Edit /tmp/todo-relay.env: set TODO_OIDC_CLIENT_SECRET and adjust paths for local dev
set -a; source /tmp/todo-relay.env; set +a
cargo run -p todo-relay
```

Or inline:

```bash
TODO_RELAY_BIND=127.0.0.1:3000 \
TODO_RELAY_PUBLIC_URL=http://127.0.0.1:3000 \
TODO_OIDC_ISSUER=https://auth.yourdomain.cc \
TODO_OIDC_CLIENT_ID=todo-relay \
TODO_OIDC_CLIENT_SECRET=<your-client-secret> \
TODO_RELAY_DATABASE=/tmp/todo-relay.db \
cargo run -p todo-relay
```

## Deploy the relay

On your VPS (behind Caddy for TLS):

```bash
cargo build --release -p todo-relay
sudo useradd -r -s /usr/sbin/nologin todo-relay
sudo mkdir -p /var/lib/todo-relay /etc/todo-relay
sudo cp target/release/todo-relay /usr/local/bin/
sudo cp relay/.env.example /etc/todo-relay/env   # edit with real values
sudo cp relay/todo-relay.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now todo-relay
```

Caddy config (`relay/Caddyfile`):

```
relay.yourdomain.com {
    reverse_proxy 127.0.0.1:3000
}
```

### Authelia setup

Register an OIDC client in your Authelia `configuration.yml`:

```yaml
- client_id: todo-relay
  client_name: Todo Sync Relay
  client_secret: <generate with: authelia hash generate argon2>
  authorization_policy: one_factor
  redirect_uris:
    - https://relay.yourdomain.com/auth/callback
    - http://127.0.0.1:3000/auth/callback    # local dev
  scopes:
    - openid
    - profile
    - email
    - groups
  grant_types:
    - authorization_code
```

Users must be in an allowed group (default: `todo-users`):

```yaml
# users_database.yml
users:
  alice:
    password: <argon2 hash>
    email: alice@example.com
    groups:
      - todo-users
```

## How it works

```
┌─────────────┐                      ┌─────────────┐                      ┌─────────────┐
│  Machine A  │  ── POST /push ────► │    Relay    │ ◄─── GET /pull ────  │  Machine B  │
│  (SQLite +  │  ◄─── GET /pull ──── │  (SQLite +  │ ── POST /push ────►  │  (SQLite +  │
│   events)   │                      │   events)   │                      │   events)   │
└─────────────┘                      └──────┬──────┘                      └─────────────┘
                                            │
                                     ┌──────┴──────┐
                                     │   Authelia   │
                                     │  (OIDC IdP)  │
                                     └─────────────┘
```

1. Every CLI command appends events to the local SQLite event log. No `todos` table.
2. `todo sync` pushes all local events to the relay and pulls events from other devices.
3. The relay deduplicates by `event_id` and serves events per user, excluding the
   requesting device.
4. Each machine folds the full event log to compute current state. Same events →
   same state, always.
5. Conflicts resolve by last-write-wins (event timestamp). For a single-user todo
   app, this is correct.

## Project structure

```
todo/                    # Cargo workspace
├── cli/                 # todo binary — the CLI
├── common/              # todo-common library — wire types (HTTP contract)
└── relay/               # todo-relay binary — the sync server
```

## Development

```bash
cargo build --workspace     # build everything
cargo test --workspace      # run all tests (36)
cargo build -p todo         # build CLI only
cargo build -p todo-relay   # build relay only
```

## License

MIT
