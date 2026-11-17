# Require `[auth]`, an https key set URL and one scope per ability

## Overview

eventkit-bridge 0.10.0 makes the bearer-token gate part of every config. Until 0.9.0 the absence of the `[auth]` table switched the token check off, which left the network bind and the `Host` check as the whole access control. From 0.10.0 a config without `[auth]`, or without one of its required keys, fails to load and names what is missing, and there is no code path that serves a `/v1` route without a validated token. `/healthz` stays the only route without one.

The same release splits the single `write` scope of calendars and reminders by ability, as mail already is: `calendar.create`, `calendar.edit` and `calendar.delete` replace `calendar.write`, and `reminders.create`, `reminders.edit` and `reminders.delete` replace `reminders.write`. The old names open nothing, and no scope implies another.

It also accepts the provider's key set URL over `https` only. RFC 8414 section 2 requires `https` for `jwks_uri`, and a key set fetched over plain `http` can be replaced on the way. There is no exception for loopback or tests.

## Skills to invoke

- `rust-style` - every Rust file.

## Non-goals

- Any change to token validation, the JWKS cache, the mail scopes or the responses.
- The token request example in the README.
- Accepting `calendar.write` or `reminders.write` during a transition. A client that needs both names carries both until the new bridge runs.
- Keeping the old behaviour behind a setting. A config without `[auth]` is an error, and the release notes say so.

## Context

- `[auth]` is optional in `src/config.rs` (`RawConfig.auth`, `Config.auth: Option<AuthConfig>`), in `src/main.rs` (`authenticator` returns an `Option`, the JWKS refresher is spawned only with it, `--check-config` prints `auth: off`), in `src/server.rs` (`App.auth: Option<Arc<Authenticator>>`, the early return in `require_bearer`) and in the README (security model, first run, config table, Authentication).
- `parse_jwks_url` in `src/config.rs` accepts `http` and `https`.
- The cask caveats in `.github/workflows/release.yml` repeat the README first-run steps and change with them.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- `mise run check` passes after every task. Tests that covered a config without `[auth]` or an `http` key set URL are turned into the failure case, never left skipped.
- Every test that drives the router without testing the `401` itself sends a valid token, through one helper.

## Implementation Steps

### Task 1: An https key set URL

**Files:**
- Modify: `src/config.rs`

- [x] `parse_jwks_url` accepts only `https`; the error names `auth.jwks_url` and says the scheme must be https
- [x] tests: an `http` URL, loopback included, fails to load with that error; `https` loads
- [x] `mise run check`

### Task 2: `[auth]` is required

**Files:**
- Modify: `src/config.rs`, `src/auth/mod.rs`, `src/server.rs`, `src/main.rs`

- [x] `Config.auth` is an `AuthConfig`; a config without `[auth]` fails with an error that names the table and its required keys
- [x] `App` holds an `Arc<Authenticator>`; `require_bearer` has no path without it; `/healthz` stays exempt
- [x] `main.rs` always builds the authenticator and the JWKS refresher; `--check-config` always prints the auth block
- [x] tests: a test helper gives every router a valid token; only the `401` tests send none; a config without `[auth]` fails naming it
- [x] `mise run check`

### Task 3: One scope per ability for calendars and reminders

**Files:**
- Modify: `src/auth/mod.rs`, `src/server.rs`

- [x] the scope table maps `POST`, `PATCH` and `DELETE` of events and reminders to `create`, `edit` and `delete`; `PATCH /v1/reminders/{id}` serves both the update and the completion
- [x] tests: every route and method against the table, each scope opens only its route, a token with the old `write` scopes gets `403` on every write route
- [x] `mise run check`

### Task 4: Docs and version

**Files:**
- Modify: `README.md`, `CLAUDE.md`, `.github/workflows/release.yml`, `Cargo.toml`, `Cargo.lock`

- [x] README: security model, first run, config example and table, Authentication, `--check-config`, an upgrade note and a troubleshooting entry
- [x] the first-run steps in the cask caveats match the README
- [x] CLAUDE.md: `[auth]` is required
- [x] version 0.10.0 in `Cargo.toml` and the README health example
- [x] `mise run check`, `shellcheck scripts/*.sh`
- [x] move this plan to `docs/plans/completed/`

## Post-Completion

- Before upgrading a running bridge without `[auth]`: register the bridge's audience and the clients in the provider, give every client a token, add `[auth]` with an `https` `jwks_url`, run `eventkit-bridge install`, then upgrade.
- Before upgrading a bridge whose `jwks_url` is `http`: change it to the provider's `https` URL first.
