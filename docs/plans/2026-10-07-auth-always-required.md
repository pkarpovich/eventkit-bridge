# Remove the `[auth] required` switch

## Overview

eventkit-bridge 0.8.0 removes the transition setting `required` from the `[auth]` table. 0.7.0 shipped it so clients could start sending tokens one by one: with `required = false` a request without `Authorization` passed as `anonymous`. Once every client sends tokens, the switch is only a way to switch the gate off by accident. From 0.8.0 a configured `[auth]` always refuses a request without a token (`401`), and the code path for anonymous requests is gone.

Without an `[auth]` table nothing changes, as before.

## Skills to invoke

- `rust-style` - every Rust file.

## Non-goals

- Any other change to token validation, scopes, the JWKS cache or the responses.
- Accepting `required` for compatibility. A config that still has the key fails to load with the existing unknown-key error, which names it; the release notes tell users to delete the line first (on 0.7.0 `required = true` is the default, so deleting it is a no-op there).

## Context

- `required` lives in `src/config.rs` (`RawAuth` field `required: Option<bool>`, `AuthConfig.required` defaulting to `true`), in `src/auth/mod.rs` (`Authenticator.required` and its accessor), in `src/server.rs` (the `Err(AuthError::Missing) if !auth.required()` branch that lets a request through as `anonymous`), in `--check-config` output, in the request log (`client=anonymous`), in tests, and in README/CLAUDE.md.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- `mise run check` passes after every task. Tests that covered `required = false` are deleted or turned into the "always required" case, never left skipped.

## Implementation Steps

### Task 1: Drop the switch

**Files:**
- Modify: `src/config.rs`, `src/auth/mod.rs`, `src/server.rs`, `src/main.rs`

- [x] remove `required` from `RawAuth` and `AuthConfig`; a config with `auth.required` is now an unknown-key error
- [x] remove `Authenticator::required` and the anonymous branch in the middleware; a missing token is always `401` with `WWW-Authenticate: Bearer`
- [x] remove `required` from `--check-config` and the startup `auth on` log line; the request log no longer produces `client=anonymous`
- [x] tests: `[auth]` with `required = true` and with `required = false` both fail to load naming the key; a request without a token is `401` on every route group; `/healthz` still needs no token; every remaining auth test passes unchanged
- [x] `mise run check` (mise unavailable; ran cargo fmt --check, clippy -D warnings, cargo test directly)

### Task 2: Docs and version

**Files:**
- Modify: `README.md`, `CLAUDE.md`, `Cargo.toml`

- [ ] README: remove `required` from the config example and table, the rollout paragraph and any `anonymous` log mention; state that a configured `[auth]` is always enforced; a short upgrade note (delete `required` from the config before upgrading)
- [ ] CLAUDE.md: drop the transition-switch wording
- [ ] version 0.8.0 in `Cargo.toml` and the README health example
- [ ] `mise run check`, `shellcheck scripts/*.sh`
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

- Before upgrading a running bridge: delete the `required` line from `[auth]` (0.7.0 treats its absence as `true`), run `install`, confirm clients still work, then upgrade.
