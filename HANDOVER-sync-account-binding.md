# Handover: reject sync account and relay switches

## Approved decision and scope

Fix finding two in `CODE_REVIEW_FINDINGS.md`: changing credentials currently reuses the previous account's local history and pull cursor.

The user approved keeping one local dataset and rejecting account switches for now. Do not automatically select a different database after login, add profiles, or implement data migration between accounts.

Keep `~/.todo/todos.db` as the source of truth. Persist an immutable sync binding for that dataset: the normalized relay base URL and its verified `user_id`. Authentication credentials are temporary; this binding survives logout and session expiry.

This is a handover only. No implementation changes have been made for this finding.

## Local-first invariants

- Adding, listing, editing, completing, removing, and inspecting local todos must not require a relay, credentials, or network access.
- A dataset can contain offline-created events before its first login. First binding must retain those events and allow them to sync to the chosen account.
- Authentication must not change the local database path, replace the dataset, or hide its todos.
- Once bound, the dataset may sync only with the same relay/account pair. A valid token for a different account is still a mismatch.
- Logout removes credentials, not events, the binding, device identity, or pull progress.
- Never repair a mismatch by resetting only the cursor: that still uploads the existing history to the wrong account.
- No automatic unbind/rebind, overwrite-binding flag, or compatibility path that permits a switch.

## Confirmed problem and current code map

The preceding review observed both defects using actual CLI/relay binaries and disposable fixtures:

1. Normal logout from Alice, login as Bob, then sync published Alice's private task into Bob's account.
2. Switching to another relay retained an old cursor of `3`, skipped that relay's existing sequence-`1` task, and reported successful sync.

Those are historical observations, not commands rerun while writing this handover. Fixtures were removed; recreate disposable fixtures for implementation verification.

Line references describe the code inspected for this handover. Navigate by symbol if they shift.

| File / symbol | Responsibility and required change |
| --- | --- |
| `cli/src/db.rs:39–42`, `sync_state` | Store the dataset binding alongside existing local sync metadata; no new todo table or remote store. |
| `cli/src/db.rs:176–197`, `get_state`, `set_state` | Add typed binding reads and an atomic compare-or-first-bind operation. Do not use independent writes for the binding's two fields. |
| `cli/src/sync.rs:86–106`, `login` | Verify the candidate session's identity, check the dataset binding, then persist credentials. Needs access to the local database. |
| `cli/src/sync.rs:118–151`, `save_session`, `confirm_login` | Do not save before a successful, fallible `/me` result and binding check. Print login success only after persistence succeeds. |
| `cli/src/sync.rs:165–185`, `sync` | Verify `/me` and enforce the binding before either pull or push; only then use the cursor. |
| `cli/src/sync.rs:247–258`, `logout` | Preserve binding and cursor. Existing remote-revocation failure handling is a separate finding. |
| `cli/src/main.rs:146–159` | Pass the already-open database to login; retain the existing database location for every command. |
| `cli/src/config.rs:8–18` | Continue storing relay URL, token, and device ID. Do not introduce a second authoritative account binding here. |
| `common/src/wire.rs:54–59`, `MeResponse` | Already provides verified `user_id` and optional display email. No wire change needed. |
| `relay/src/api.rs:176–188`, `/me` | Already returns the identity derived from the authenticated session. No relay change needed for this fix. |
| `README.md`, sync/login documentation | Document first binding, rejection of a different identity, and offline operation after logout. |

## Chosen design

### 1. Persist identity separately from credentials

Use the existing `sync_state` table for:

```text
sync_relay_url
sync_user_id
last_pulled_seq
```

The first two keys are the binding; the existing cursor belongs to that binding. Keep the session token in the existing config file, not in SQLite.

Use the UUID returned by authenticated `/me`, not email, OIDC display name, session token, or device ID. Email changes must not look like an account switch; two distinct users sharing an email must not compare equal.

Represent binding reads as absent, complete/valid, or corrupt. One missing key or an invalid UUID is an error for login/sync, not permission to silently create a new binding. Do not validate this metadata in database initialization in a way that prevents offline local commands.

Use one relay-URL normalization rule for identity comparison and HTTP request construction. Parse with the existing URL facilities; retain scheme, host, effective port, and deployment base path, while treating equivalent trailing slashes consistently. Do not equate different relays using email, DNS resolution, redirects, or hostname guesses. Different aliases or an HTTP-to-HTTPS change are not automatic permission to switch the binding.

### 2. Make first binding atomic and subsequent binding immutable

Add a small database operation that, within a SQLite transaction:

1. Reads the current binding again.
2. Creates both binding fields only when the dataset is eligible for first binding.
3. Accepts an exact existing match without changing binding or cursor.
4. Rejects a mismatch or corrupt binding without mutation.

Initialize a genuinely new binding's cursor to `0` in the same transaction. Do not carry an unexplained pre-existing cursor into a fresh binding. Legacy databases need the separate upgrade handling below.

Concurrent first-login attempts for different identities must not overwrite each other: one may establish the binding, and the other must fail. Keep HTTP requests outside the database transaction.

Persist the binding before saving candidate credentials. SQLite and the config file do not share a transaction: if config saving subsequently fails, leave the successfully established binding intact, return failure, and allow another login to that same identity. Do not roll it back to an unbound state or report success. No event transfer occurs during login.

### 3. Enforce the binding during login

For an already-bound dataset:

1. Read/validate the local binding. An obviously different normalized relay can be rejected before opening the browser.
2. Complete the existing browser/callback flow.
3. Query `/me` with the candidate token and require a successful, valid `MeResponse`.
4. Compare `(relay_url, me.user_id)` against the binding, rechecking atomically before any persistence.
5. Only on a match save the candidate credentials and print success.

A first login to a genuinely unbound, never-synced dataset follows the same verification, then establishes its first binding without changing local events.

An account/relay mismatch, HTTP failure, malformed identity response, or failed authentication must leave previous credentials, events, binding, device identity, and cursor untouched. Return a nonzero CLI exit. Report the bound relay/user UUID and the reason without logging tokens. Email is optional display information, not identity.

The fallible identity-confirmation result is also required by `HANDOVER-login-callback.md`. Reuse that result/helper if its fix has landed. This binding check does not replace per-attempt callback state: an attacker token can pass `/me`, especially before the first binding. Keep callback-state implementation in its separate handover.

### 4. Enforce the binding again before sync

Login-only enforcement is insufficient: stored credentials can be stale, copied, or manually changed.

Before `/pull`, cursor changes, or `/push`:

1. Load the configured credentials and normalize the relay URL.
2. Reject a local relay mismatch or corrupt binding immediately.
3. Verify the active token using authenticated `/me` on that relay.
4. Check that the returned UUID matches the dataset binding. For eligible unbound/legacy datasets, establish their first binding under the rules in this file.
5. Only then execute the existing pull/apply/push flow.

Use one fallible authenticated-identity request path for login and sync rather than keeping a success-swallowing confirmation path. A sync handshake failure must produce no event transfer and no progress update.

This adds a request to an operation that already requires network access. It must not add requests to local commands or cause offline command dispatch to check authentication.

### 5. Preserve binding on logout and reauthentication

Logout continues to remove local credentials, but must not remove any database binding or cursor metadata. Reauthentication to the same relay/user resumes existing progress, including after token expiry, config-file removal, or restoring the database onto another machine.

`whoami` can continue displaying the active session identity; it must not bind or rebind the dataset as a side effect. A later sync still performs its own enforcement.

## Existing-database upgrade policy

Current databases do not record the account that owned their history. Event IDs, device IDs, timestamps, and sequence numbers cannot reconstruct that identity. An earlier accidental account switch may already have mixed history; this fix cannot retroactively detect or undo that leakage.

Use conservative, non-destructive upgrade handling:

- A dataset with no binding, no prior pull-cursor entry, and no evidence of a prior configured sync relationship is eligible for ordinary first binding, even if it contains offline-created todos.
- For an unbound legacy dataset with stored credentials, verify the existing configured session through `/me` before replacing it with a candidate login. If a candidate login is attempted, it must match that established relay/user identity. Do not bootstrap ownership from the replacement token alone.
- Once the existing configured identity is verified, adopt it as the binding atomically and reset the previously unscoped cursor to `0` once. Replay from that same account is safe because events deduplicate by `event_id`; subsequent matching logins must preserve progress.
- If there is evidence of prior sync/configuration but the original identity cannot be verified because credentials are absent, expired, or invalid, fail login/sync with a migration-required diagnostic. Keep all local data and local commands usable. Do not guess identity, accept a new account silently, delete data, or offer an automatic rebind escape hatch.

The last case is a real information limitation: the old format did not retain the needed identity. Recovery requires verified prior-account information or a separately approved legacy-adoption procedure. Do not hide that limitation behind an unsafe fallback. Designing profiles, migration commands, or an ownership-reset procedure is outside this approved fix.

A complete binding that already exists in the database remains authoritative even if the config file is missing or its token has expired; same-identity reauthentication does not need the old token.

## Acceptance and regression coverage

Use deterministic, isolated tests for observable account/data boundaries. Do not pin exact error wording or test mere helper forwarding. Config-sensitive checks should use subprocess-specific HOME directories rather than mutating process-global HOME in parallel tests.

1. Offline todos created before first login survive first binding and sync to the verified chosen account; no database-path switch occurs.
2. Logout and unavailable/expired sessions do not prevent local add/list/edit/remove operations. Logout preserves events, binding, device identity, and cursor.
3. A new valid token for the same relay/user succeeds and preserves the cursor. Email changes do not affect this comparison.
4. A valid token for another user on the same relay is rejected before credential replacement or event transfer. Cover distinct UUIDs with identical email values.
5. A different relay is rejected, including one containing lower sequence numbers. The old binding, credentials, cursor, and event set remain unchanged.
6. Manually substituted credentials cannot bypass sync enforcement: a mismatched `/me` identity results in no pull/push requests and no local state mutation.
7. `/me` 401, malformed JSON, and transport failures fail without saving candidate credentials, updating progress, or claiming success.
8. Partial/corrupt binding metadata fails closed for login/sync while local todo commands remain usable.
9. Concurrent different-identity first bindings cannot overwrite one another. A credential-save failure after binding leaves the binding protected and does not report successful login.
10. Legacy adoption requires a verified existing configured identity; the first adoption resets the unscoped cursor only once. A missing/unverifiable legacy identity does not silently bind to a candidate account or erase data.

Update affected contract tests and all callers if signatures change. Use language-server references before changing exported symbols when a server is available.

## Runtime verification and delivery

Before implementing, reproduce the account switch and stale-cursor scenarios in disposable HOME directories using two test accounts and two local relays. Do not use real private todos or production credentials.

After implementation:

- Run the applicable regression tests and workspace build/test checks after edits are complete.
- Exercise actual CLI and relay binaries with a local signed test IdP or a development IdP; tests alone are not proof.
- Create a private task under account A, sync, logout, and attempt account B. Observe rejection, unchanged local data/progress, and no A task appearing in B's relay history.
- Reauthenticate as A and verify sync resumes normally.
- Attempt a different relay containing an existing low-sequence task; verify rejection rather than silent cursor reuse or upload of the old dataset.
- Stop the relay and exercise local commands both before first binding and after logout/binding. Observe that they still work without network access.
- Exercise substituted credentials and legacy-upgrade failure/adoption paths using isolated fixtures.
- Remove throwaway fixtures and stop test-owned services. Update existing README sync/login documentation and affected callers; do not retain an unguarded old login/sync path.
- Report exact exercised scenarios, outcomes, changed files, and any genuinely missing legacy-identity prerequisite.

## Non-goals

Do not fold in callback-state implementation, logout-revocation error handling, device-ID unification, event deserialization fixes, push batching, relay changes, profiles, remote storage of local state, or account migration. Those are separate findings or future product decisions. Only the mandatory fallible `/me` verification shared with the callback handover is a prerequisite for this fix.
