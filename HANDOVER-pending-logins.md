# Handover: bound and expire pending relay logins

## Goal and scope

Fix finding 4 from the repository review: unauthenticated requests to `/auth/login` accumulate abandoned pending login state indefinitely.

The user approved the design below and requested this handover. This document records the implementation plan only; no implementation changes have been made for this finding.

Keep this change scoped to pending-login expiry, capacity, and callback URL size. Do not implement the separate CLI callback-binding, account-isolation, callback socket timeout, credential-permission, logout, or sync fixes. The existing handovers for those findings remain separate.

## Confirmed problem

For every accepted `/auth/login?cli_callback=<loopback-url>` request, the relay generates fresh OIDC state and nonce, inserts an entry into `pending_logins`, and redirects the requester to the identity provider. Allocation happens before authentication.

An attacker can repeatedly make the initial request and ignore its redirect. No relay account, identity-provider login, or real listener at the callback address is required. Reusing the same callback URL still creates separate entries because each request receives fresh state.

The map has no expiry or capacity limit. `PendingLogin` contains only the nonce and callback URL. An entry is removed only when `/auth/callback` receives its exact state, or when the relay process exits. Ordinary abandoned logins therefore accumulate too.

The CLI's five-minute login timeout only stops its local wait. It does not notify the relay or remove server-side pending state. The callback's current message, "invalid or expired state", does not mean expiration is implemented.

### Historical verification

The repository review exercised an isolated relay with a local signed OIDC provider. A bounded batch of 1,200 abandoned unauthenticated login requests, using long valid loopback callback URLs, increased relay RSS by roughly 10 MiB. No authentication was completed and no attempt was made to exhaust memory or crash the relay.

These are historical observed results, not a scenario rerun while writing this handover. Review fixtures were removed. The handover author checked the current source.

### Impact and limits

Continued requests can exhaust relay memory and deny service. This finding does not itself disclose credentials, bypass JWT verification, or grant access to another user's events.

Expiry limits how long a flow remains usable; a capacity limit is also necessary because a high request rate can create many unexpired entries. A callback size limit bounds the variable-sized data retained in each entry. Together these bound retained pending-login state, not all process memory or request-processing costs.

## Current code map

Line references describe the source checked when this handover was written; navigate by symbol if they shift.

| File / symbol | Relevant behavior and intended change |
| --- | --- |
| `relay/src/auth.rs:25–35`, `AppState::pending_logins`, `PendingLogin` | Existing `Arc<Mutex<HashMap<String, PendingLogin>>>`. Keep this storage and add a monotonic expiration deadline to each entry. |
| `relay/src/api.rs:24–57`, `auth_login` | Validates the callback, generates state/nonce, inserts without a bound, and redirects. Add callback-size validation, expired-entry pruning, and atomic capacity admission. |
| `relay/src/api.rs:65–87`, `auth_callback` | Removes the matching entry before code exchange. Preserve single-use removal and reject expired entries before exchange. |
| `relay/src/main.rs`, `AppState` initialization | Initializes the empty map. No periodic task or architectural change is required by the chosen design. |
| `cli/src/sync.rs:81–82`, `LOGIN_TIMEOUT` | Existing five-minute client timeout. Leave unchanged; it motivates the initial server-side lifetime. |
| `README.md` | After implementation verification, document the login lifetime, bounded admission, and retry behavior in existing documentation. |

## Chosen design

Use a bounded, expiring map with lazy cleanup. Retain the existing standard-library map and mutex; do not add a cache dependency, database table, shared wire type, or environment configuration solely for this fix.

Initial policy:

| Control | Value | Purpose |
| --- | --- | --- |
| Pending-login lifetime | 5 minutes | Bound flow validity; align with the current CLI timeout. |
| Maximum pending entries | 1,024 | Bound the number of retained flows. |
| Maximum decoded callback URL length | 2,048 bytes | Bound the variable-sized string retained per flow. |

Use named constants near the pending-login logic. These are conservative initial values for this small relay, not a claim that they are optimal for every deployment.

### 1. Monotonic expiration

Add `expires_at: std::time::Instant` to `PendingLogin`. Calculate the deadline when a flow is admitted. A flow is expired when `expires_at <= now`; only entries with a later deadline are live.

Use a monotonic clock because this is in-process state: wall-clock adjustments must not extend or prematurely expire pending flows. Pending state remains ephemeral and is not persisted.

### 2. Bounded admission in `/auth/login`

After extracting `cli_callback`:

1. Check the decoded string's byte length before URL parsing or pending-state insertion. Reject values over 2,048 bytes with HTTP 400. Preserve current missing-parameter and loopback-validation behavior.
2. Preserve the complete validated callback URL, including any query parameters. This must coexist with the separate CLI callback-binding handover; do not strip state or impose a new callback path rule.
3. Acquire the existing pending-map mutex and capture the current monotonic time.
4. Remove expired entries, for example with `retain` and the common expiry predicate.
5. If 1,024 live entries remain, reject the new login with HTTP 503. Do not create another stored flow or evict an active one.
6. Otherwise admit the flow with fresh OIDC state, nonce, callback URL, and expiration deadline. Keep pruning, capacity checking, and insertion in the same critical section so concurrent requests cannot exceed the cap.
7. Release the mutex before returning the existing authorization redirect.

OIDC authorization URL generation is local work. Keep the implementation simple, and never hold the pending-map mutex across network calls or an `await`. Preserve the current secure state/nonce generation and mutex-error handling.

Capacity rejection should give a brief, non-sensitive message that login capacity is temporarily exhausted and the caller should retry. HTTP 503 represents shared service capacity exhaustion; rate-limit responses, if implemented separately, can use HTTP 429.

### 3. Expiration enforcement in `/auth/callback`

Keep the current remove-first behavior: look up and remove the submitted state under the mutex, then release the mutex.

- Missing state: preserve HTTP 400 with the existing invalid/expired-state response.
- Expired entry: return that same HTTP 400 response before authorization-code exchange, user creation, or session creation.
- Live entry: continue through the existing code exchange, ID-token verification, group check, and session creation.

Never reinsert a consumed entry after failure. A callback is single-use whether authentication succeeds or fails. Do not extend the lifetime when a callback arrives. Expiry is checked at callback admission; this change does not require canceling an already admitted token exchange when its original deadline subsequently passes.

### 4. Cleanup and remaining availability tradeoff

Pruning on insertion is the chosen minimal implementation. Expired entries may remain allocated while the service is idle, but their number and retained callback bytes stay bounded, and they cannot authenticate. The next login admission reclaims them before checking capacity.

A periodic cleanup task is optional future work for promptly releasing idle allocations, not required for this fix. Do not add one as part of the initial implementation.

An attacker can still occupy the bounded slots and temporarily deny legitimate login attempts. Login rate limiting is a useful secondary defense, but is separate from this implementation. Rate limiting alone does not fix indefinite accumulation. Any future client-IP policy must account for the relay's reverse-proxy deployment and trusted forwarding headers rather than treating every request as the proxy's loopback IP.

## Implementation and verification plan

1. Recheck the handlers, `PendingLogin` construction sites, and any intervening changes before editing. Use language-server references before exported-symbol changes when a server is available.
2. Add the constants and monotonic deadline, enforce callback size, and implement expiry/capacity admission under the existing mutex. Update every `PendingLogin` constructor; do not add a compatibility path that omits expiry.
3. Enforce callback expiry before code exchange while preserving state consumption and all existing authentication safeguards.
4. Add deterministic regressions for expiry boundaries, admission bounds, and single-use state. Follow existing Rust test conventions and keep any helper narrowly scoped; avoid a generic cache framework or production clock override solely for testing. Exercise controlled `Instant` values rather than using sleep-based tests.
5. Run relevant tests, then exercise actual relay endpoints against a disposable database and local OIDC provider. Do not flood the deployed relay or use the user's real credentials or database.
6. After runtime proof, update existing README documentation and remove disposable fixtures/scripts. Run workspace verification once after implementation is integrated.

### Required behavioral checks

- A normal live flow still redirects and completes login through the existing OIDC verification path.
- A flow just before its deadline is accepted; one at or after the deadline is rejected without code exchange or session creation.
- Accepted and rejected-expired callbacks both consume their matching entry; replay cannot reuse that state.
- At full live capacity, a new login returns 503 without evicting any live flow or increasing the entry count.
- Expired entries are reclaimed before capacity is checked, allowing a subsequent legitimate admission.
- Concurrent admissions cannot exceed the configured cap.
- A valid loopback callback of exactly 2,048 decoded bytes is eligible for admission; one byte over is rejected with 400 without allocation in the map. Existing loopback restrictions still apply independently of length.
- Callback query parameters survive unchanged, including a CLI login state added by the separate callback-binding fix.
- A bounded abandoned-login smoke load reaches the cap and starts receiving 503 responses rather than continuing to add pending entries. Do not attempt an out-of-memory demonstration.

Memory measurements are supporting smoke evidence, not the sole assertion: allocators can retain freed pages, so RSS need not fall immediately after pruning. Verify the actual admission and expiry behavior rather than relying on a noisy RSS threshold.

## Acceptance criterion

Unauthenticated callers cannot retain more than 1,024 pending flows, each with at most a 2,048-byte callback URL. Each flow is valid for at most five minutes from admission, stale callbacks cannot initiate token exchange or session creation, and every consumed state remains single-use. Normal authentication, callback preservation, existing loopback restrictions, and JWT/group checks remain intact. No unrelated review finding is implemented as part of this change.
