# Handover: bind CLI login callbacks to their initiating attempt

## Goal and scope

Fix the first finding from the repository review: the CLI loopback callback accepts an unrelated session token, allowing attacker-session substitution.

The user wants findings addressed one at a time. Implement only this login-binding fix and its validation-before-persistence behavior. This handover does not authorize fixing other findings: account/relay database isolation, callback read deadlines, pending-login expiry, credential-file permissions, logout revocation, event serialization, or display IDs.

This file is a handover only. No implementation changes have been made for this finding.

## Confirmed problem

Current flow:

1. CLI binds an ephemeral port on `127.0.0.1` and prints its relay login URL.
2. `CallbackServer` accepts any request target containing a `token` query parameter. It does not check the HTTP method, exact callback path, or a CLI-generated state.
3. CLI saves that token and relay URL before calling `/me`.
4. `/me` errors are swallowed and still produce a successful login message.
5. Later sync uploads the complete local event history under the saved token's account.

The reviewing agent exercised actual CLI and relay binaries using disposable HOME directories, SQLite databases, and a local OIDC provider with signed ID tokens:

- A request to `/not-the-callback?token=<attacker-session>` made the victim CLI complete login successfully.
- The next victim sync uploaded a private note that the attacker could pull from their account.
- An invalid token also made login exit successfully; subsequent `whoami` failed with HTTP 401.

These are historical observed results, not commands run by the handover author. Review fixtures were removed; recreate disposable fixtures when verifying the fix.

### Threat model

The attacker has a valid allowed-user session on the same relay and can discover/connect to the victim's ephemeral loopback port while login is pending, but cannot read the fresh CLI login state.

A valid attacker token passes `/me`. Token validity alone therefore does not bind the response to the initiating CLI login. A random port and the relay's OIDC state/nonce checks do not protect this separate final delivery step.

The proposed fix does not protect against malware that can read the same user's terminal, process memory, or credential file. The printed fallback login URL will contain the state and must be treated as sensitive during the pending attempt.

## Current code map

Line references describe the code reviewed when this handover was created; navigate by symbol if they shift.

| File / symbol | Current responsibility and required change |
| --- | --- |
| `cli/src/callback.rs:18–34`, `CallbackServer::{bind,callback_url}` | Own the listener and port. Add fresh per-attempt state and include it in the callback URL. |
| `cli/src/callback.rs:40–101`, `wait_for_token`, `handle_connection` | Accept connections and return the first token. Validate request method/path/state/token before accepting a callback; rejected requests must not consume the attempt. |
| `cli/src/callback.rs:129–150`, `extract_token`, `url_decode` | Loose extraction/manual decoding. Replace with proper URL/query parsing and remove obsolete helpers. |
| `cli/src/callback.rs:153–200`, callback HTML | Currently claims login is complete before `/me` and saving. Keep browser messaging truthful about receipt versus completed authentication. |
| `cli/src/sync.rs:86–106`, `login` | Currently calls `save_session` before `confirm_login`. Reverse that dependency. |
| `cli/src/sync.rs:110–123`, `build_login_url`, `save_session` | Carry the callback URL intact through the outer `cli_callback` query parameter; save only after successful validation. |
| `cli/src/sync.rs:127–151`, `confirm_login` | Currently returns `()` and hides HTTP/JSON/network failures. Return a fallible authenticated identity result. |
| `relay/src/api.rs:24–57`, `auth_login` | Already stores the complete callback URL in `PendingLogin`. Preserve that behavior and existing loopback validation. |
| `relay/src/api.rs:139–141`, `auth_callback` | Appends `?token=` manually. Use URL query construction so an existing state query is retained. |
| `cli/src/callback.rs:204–312`, callback tests | Several tests currently accept stateless/empty-token callbacks. Replace obsolete expectations with behavioral security regressions. |
| `cli/src/sync.rs`, login URL tests | Update affected contract tests for the callback's nested query string. |
| `README.md` | Update existing login documentation after runtime verification. No new changelog is needed if none exists. |

## Chosen design

### 1. CLI-owned, fresh state

Generate an unpredictable state for each new `CallbackServer`. `Uuid::new_v4()` is already available in the CLI and provides sufficient randomness for this purpose; do not derive state from the port, device ID, timestamp, or session token.

Store expected state in memory for that server instance. Its callback URL becomes:

```text
http://127.0.0.1:<port>/callback?state=<fresh-random-value>
```

The relay's existing OIDC state and nonce remain separate and unchanged. This is the CLI-to-relay-to-CLI binding, not a replacement for OIDC validation.

### 2. Preserve state through the relay redirect

Keep using the existing `cli_callback` parameter and stored callback URL. A new shared wire type or separate relay state field is unnecessary for this design.

Parse the stored callback with `url::Url` and append the token using query-pair APIs. The result must be:

```text
http://127.0.0.1:<port>/callback?state=<fresh-random-value>&token=<session-token>
```

Do not concatenate another `?`. Preserve existing callback query pairs with correct encoding and leave the relay's loopback-only validation intact.

### 3. Validate before accepting a callback

Use proper URL/query parsing rather than the existing replacement-based decoder. The CLI already depends on `reqwest`, which exposes `reqwest::Url`; follow existing dependencies rather than adding an HTTP framework solely for this fix.

Accept a token only when all of these hold:

- HTTP method is `GET`.
- The request uses the expected local callback target with exact path `/callback`.
- There is exactly one `state` parameter, matching the server's expected state.
- There is exactly one `token` parameter and its value is non-empty.

Reject missing, empty, incorrect, or duplicate state; missing, empty, or duplicate token; and wrong paths/methods. Do not allow duplicate-parameter ordering to decide which credential or state wins.

A rejected callback must not return a token, save credentials, show completed-login success, or end the pending attempt. A later legitimate callback must still work within that attempt's existing lifetime.

After one matching callback is accepted, stop accepting and close the listener before confirmation/persistence. Do not keep the listener alive through the `/me` request. State is one-attempt-only; never provide a stateless compatibility fallback.

Avoid a general HTTP-server rewrite or unrelated parser/timeout hardening. Do not add extra state/token logging or include credentials in error messages.

### 4. Validate token before persistence

Change the CLI sequence to:

```text
receive matching callback -> successful authenticated /me -> save credentials -> print success
```

Make `confirm_login` return a `Result<MeResponse, SyncError>` or the equivalent existing error/result shape. Require a successful response and a valid `MeResponse`; HTTP errors, JSON parse failures, and transport failures must propagate as login failures.

Do not write `sync_url`, `session_token`, or a new device ID until confirmation succeeds. An authentication/confirmation failure must leave any previous config untouched and produce a nonzero CLI exit without a successful-login message.

Print the authenticated identity only after config saving succeeds. The callback page is currently sent before confirmation; the minimal truthful alternative is to acknowledge receipt and tell the user to check the CLI, rather than claiming login is already complete. Do not move sync/config responsibilities into the callback HTTP parser solely to produce that page.

## Acceptance and regression coverage

Keep deterministic, isolated tests for security boundaries and observable behavior, not exact HTML wording, UUID strings, or helper implementation details.

1. A callback with a valid attacker's token but missing/wrong state is rejected.
2. A rejected callback followed by a correct one still completes the same attempt.
3. A state from a different login attempt is rejected.
4. Wrong method/path, empty token, and duplicate state/token parameters are rejected. Cover duplicates in both parameter orders.
5. A correct method/path/state with one non-empty token is accepted once, and the listener is not retained after acceptance.
6. The relay redirect retains the CLI state and adds one correctly encoded token; the outer relay login URL round-trips the complete callback query.
7. A matching callback followed by `/me` 401, malformed user-info JSON, or a transport failure fails login without changing previous credentials. A fresh HOME must not gain a saved session on those failures.
8. A real successful login validates `/me`, persists the session, and authenticates `whoami` afterward.

Update broken existing contract tests. Delete tests that merely pin URL/page wording or preserve the old insecure acceptance behavior; do not re-pin incidental strings. Use isolated subprocess HOME directories for config-sensitive integration checks rather than changing process-global HOME in parallel tests.

## Runtime verification and delivery

Before implementing, reproduce the attack in a disposable environment if the earlier fixtures are unavailable. Use an attacker test account and a separate victim HOME, not real credentials or production services.

After implementing:

- Run the applicable tests and workspace build/test checks after edits are complete.
- Exercise actual CLI and relay binaries with an isolated signed test IdP or an available development IdP. Tests alone are not enough.
- While the victim CLI waits, send an attacker session with no/wrong state and verify it remains pending. Complete the genuine flow afterward and confirm `whoami` identifies the intended user.
- Exercise matching-state/invalid-token confirmation and observe failed exit plus unchanged prior config.
- Observe the browser callback page or equivalent HTTP response and the CLI result; neither should falsely claim completed authentication.
- Remove throwaway fixtures and stop test-owned services. Update existing documentation and affected callers; retain no obsolete stateless path.
- Report exact verification commands/scenarios, observed results, changed files, and any genuine blocker. Do not claim a production IdP was tested if only the local fixture was used.

Follow repository rules and use language-server references before changing exported signatures when a server is available. Keep this fix scoped to finding one; do not silently fold the remaining review findings into it.
