# Bearer-token authentication

## Overview

eventkit-bridge 0.7.0, on top of 0.6.0, adds an optional OAuth 2.0 bearer-token gate. A client presents an RFC 9068 JWT access token that an external OpenID provider issued through the `client_credentials` grant; the bridge validates it locally against the provider's JWKS and requires one scope per route. Without an `[auth]` table nothing changes: the network bind and the `Host` check stay the whole access control, as today.

```
client --POST /api/oauth2/token (client_credentials)--> provider --JWT--> client
client --Authorization: Bearer <jwt>--> eventkit-bridge --validates against--> <issuer>/jwks.json (cached)
```

The token is an additional gate, not a replacement: the bridge still refuses to bind an unspecified address and still checks `Host` before anything else.

## Skills to invoke

- `rust-style` - every Rust file.

## Non-goals

- Acting as an OAuth client or provider: no token endpoint, no login, no refresh, no OpenID discovery (`jwks_url` is explicit).
- Per-calendar, per-list or per-account scopes. Scopes are per route group; the existing config policy decides which calendars, lists and accounts a request may touch.
- Algorithms other than RS256, a configurable algorithm list, symmetric keys, opaque tokens, token introspection.
- Replay protection: `jti` and `iat` are read but not checked.
- A token in the query string or a cookie. Only the `Authorization` header is read.
- mTLS or a TLS listener. The bridge still serves plain HTTP on a private address.

## Context (verified against Authelia 4.39)

- **Header:** `{"alg":"RS256","kid":"<id>","typ":"at+jwt"}`.
- **Claims:** `iss` (the provider URL), `sub` (the client id for `client_credentials`), `client_id`, `aud` as an **array** of strings, `exp`, `iat`, `nbf`, `jti`, and the scopes in **`scp` as an array** of strings, not the RFC 9068 `scope` string. The token lifetime was 15 minutes.
- **A token requested without an `audience` parameter is still issued and has no `aud` claim at all.** The bridge must therefore reject a token whose `aud` is missing, not only one whose `aud` does not contain the configured audience.
- A scope the client is not allowed is refused by the provider (`invalid_scope`); a wrong client secret is `invalid_client`. Neither reaches the bridge.
- **JWKS** at `<issuer>/jwks.json`. Key rotation is a manual config change on the provider; a new key appears with a new `kid`.
- RFC 9068 providers put the scopes in `scope` as a space-separated string. The bridge accepts both forms.

## Design

### Config

A new optional `[auth]` table. Absent means no check, today's behaviour unchanged.

```toml
[auth]
issuer = "https://auth.example.com"
audience = "https://eventkit-bridge"
jwks_url = "https://auth.example.com/jwks.json"
# required = true
# scope_prefix = "bridge:"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `issuer` | yes | Compared exactly with the token's `iss`. Not blank, no whitespace. |
| `audience` | yes | Must be contained in the token's `aud`. Not blank, no whitespace. |
| `jwks_url` | yes | An absolute `http` or `https` URL. No discovery. |
| `required` | no | Default `true`: a request without a token is `401`. `false` is the transition setting: a token that is sent is validated and an invalid one refused, but a request without `Authorization` goes through and is logged as `anonymous`. The switch is temporary: once every client sends tokens, a later release removes the key and makes a configured `[auth]` always required. Keep it isolated (one field, one branch in the middleware) so that removal is small. |
| `scope_prefix` | no | Default `bridge:`. The scope a route needs is `scope_prefix` + the name from the table below. May be empty. Every character must be from the RFC 6749 scope-token set (`%x21 / %x23-5B / %x5D-7E`), so no space or `"` or `\`. |

Unknown keys are rejected, as everywhere in the config. The parsed table is `config::AuthConfig`.

### Token validation (`src/auth.rs`)

Only `src/auth.rs` parses and validates tokens. In order, every failure is `401`:

1. Exactly one `Authorization` header, scheme `Bearer` compared case-insensitively, a non-empty token. A different scheme, two headers or an empty token count as "a token was sent and it is invalid".
2. The header must have `alg` `RS256` and a `kid`. `none`, every `HS*` and every other algorithm are refused; `jsonwebtoken::Validation.algorithms = [RS256]` plus a decoding key built from an RSA JWK makes an `HS256` token signed with the public key fail as an algorithm mismatch, never verify. A missing `kid` is refused: with key rotation it is the only safe lookup.
3. `typ` must be present and equal, case-insensitively, `at+jwt` or `application/at+jwt`.
4. The key with that `kid` from the JWKS cache (see below) verifies the signature. `jsonwebtoken` also checks `exp` and `nbf` with `leeway = 60` and `validate_nbf = true`; `validate_aud = false` and `required_spec_claims = {"exp"}`, because the bridge checks `iss`, `aud` and the subject itself in the next step so the rules are in one place and pinned by tests rather than by library defaults.
5. Claims are decoded into the bridge's own `Claims` struct: `iss: String`, `sub: Option<String>`, `client_id: Option<String>`, `aud: Option<OneOrMany<String>>` (an `#[serde(untagged)]` enum accepting a string or an array), `scp: Option<Vec<String>>`, `scope: Option<String>`, plus `exp`, `nbf`, `iat`, `jti` as optional numbers and strings. `iss` must equal `issuer`. `aud` must be present and contain `audience`. At least one of `sub` and `client_id` must be non-empty.
6. The scope set is the union of `scp` and `scope.split(' ')`.

A valid token yields `Principal { client: String, scopes: BTreeSet<String> }`, where `client` is `client_id`, else `sub`. The raw token and the claims never leave `auth.rs`; the error type carries only a kind (`Missing`, `Malformed`, `UnknownKey`, `Invalid`), never claim values or the token.

### Scope per route

A compile-time table in `auth.rs`, keyed by method and axum route template (the `MatchedPath` the request log already reads):

| Scope name | Routes |
| --- | --- |
| `calendar.read` | `GET /v1/calendars`, `GET /v1/events`, `GET /v1/events/`, `GET /v1/events/{id}`, `GET /v1/free` |
| `calendar.write` | `POST /v1/events`, `PATCH` and `DELETE /v1/events/{id}`, `PATCH` and `DELETE /v1/events/` |
| `reminders.read` | `GET /v1/lists`, `GET /v1/places`, `GET /v1/reminders`, `GET /v1/reminders/{id}` |
| `reminders.write` | `POST /v1/reminders`, `PATCH` and `DELETE /v1/reminders/{id}` |
| `mail.read` | `GET /v1/mail/accounts`, `GET /v1/mail/messages`, `GET /v1/mail/messages/{id}` |
| `mail.junk` | `PATCH /v1/mail/messages/{id}` |
| none | `GET /healthz` |

A write scope does not imply the read scope; a client that needs both asks for both. A request that matches no route (the `404` fallback) or a method the route lacks (`405`) still needs a valid token when `required` is true, and needs no scope: the token is checked, then the request goes on to its `404`/`405`. A test walks every route registered in `router()` and fails when a method/route pair other than `/healthz` is missing from the table, so a route added later cannot ship without a scope.

### Middleware and responses

One `require_bearer` middleware, inserted between the `Host` check and the body limit, so the order of execution is request log, `Host` check, token, body limit, handler. `/healthz` is exempt inside the middleware, not by routing, so the exemption is one line next to the table.

| Case | Status | `WWW-Authenticate` | Body |
| --- | --- | --- | --- |
| no token, `required = true` | `401` | `Bearer` | `{"error":"missing bearer token"}` |
| no token, `required = false` | passes as `anonymous` | | |
| token sent and invalid for any reason in steps 1-5, or an unknown `kid` after the refetch | `401` | `Bearer error="invalid_token"` | `{"error":"invalid bearer token"}` |
| valid token without the route's scope | `403` | `Bearer error="insufficient_scope", scope="<prefix><name>"` | `{"error":"insufficient scope: <prefix><name> needed"}` |

Bodies use the existing `{"error": ...}` shape through `ApiError::Status`, with the header added. The text never names the failing rule, the claim values or the token. The `Principal`, or `anonymous`, rides on the response as an extension the way `Rows` does, so `log_request` can log it.

### JWKS cache

`auth::Jwks` holds the keys behind a `JwksSource` trait with one method, `fetch() -> Result<Vec<u8>, FetchError>`, so tests inject a scripted source and the production `HttpJwksSource` is tested on its own.

- **Startup:** load the persisted file, then fetch. A failed fetch is logged and retried every 30 s until the first success; it is never fatal.
- **Refresh:** every 12 h after a successful fetch; after a failed refresh the old keys stay and the next attempt is in 5 min.
- **Unknown `kid`:** one refetch, then the lookup is retried once; a second unknown `kid` within 60 s of the last refetch does not fetch again and is `401`. Concurrent requests with an unknown `kid` share one fetch behind a `tokio::sync::Mutex`. Timing uses `tokio::time::Instant`, so tests run with `#[tokio::test(start_paused = true)]` and `tokio::time::advance`.
- **Parsing:** `jsonwebtoken::jwk::JwkSet`; keep only keys with `kty = RSA`, `alg` absent or `RS256`, `use` absent or `sig`, and a `kid`. A body that parses to zero usable keys is a failed fetch, so a provider misconfiguration cannot empty the cache.
- **Persistence:** the last good body is written as `{"fetched_at": <unix seconds>, "jwks": <body>}` to `jwks-cache.json` next to the config file (`Config::default_path().with_file_name("jwks-cache.json")`, so `~/.config/eventkit-bridge/jwks-cache.json`), through a temp file and rename. It is loaded at startup, so a restart while the provider is down does not turn every request into `401`. A missing or unparsable file is logged and treated as absent. The file holds public keys only. The path is a constructor argument, never a config key; tests pass a `tempfile` directory.
- **`HttpJwksSource`:** `GET` with a 10 s global timeout, `http_status_as_error`, a response body cap of 64 KiB (`BodyWithConfig::limit`), and the OS trust store (`RootCerts::PlatformVerifier`), so a provider behind a private CA the Mac already trusts works. It runs on `spawn_blocking`. Every error is logged with the status or the error kind only; the body is never logged.
- **Health:** `jwks_keys` is the number of usable keys, `jwks_age_s` the seconds since `fetched_at` of the keys in use, `null` when none are loaded.

### Crates

Added with `cargo add`; the versions are the current releases checked with `cargo search` on 2026-10-06.

- `jsonwebtoken = { version = "11.1.0", default-features = false, features = ["rust_crypto"] }`. 11.x has no crypto backend in its default features; `rust_crypto` (`rsa`, `sha2`, `hmac`, `p256`, `p384`, `ed25519-dalek`) builds without cmake or a C toolchain on Linux and macOS, unlike `aws_lc_rs`. `use_pem` is not needed: the production key comes from a JWK, the test key from DER. It pulls `base64 ^0.22` next to the crate's `base64 0.23`; two copies are accepted rather than downgrading the crate's own.
- `ureq = { version = "3.4.2", default-features = false, features = ["rustls", "platform-verifier"] }` for the JWKS fetch. Chosen over `reqwest 0.13.5` (hyper, h2, tower-http and `aws-lc-rs` through its `rustls` feature, which needs cmake) and over `jwks_client_rs 0.6.8` (a wrapper over reqwest and jsonwebtoken whose in-memory cache has none of the persistence or rate limiting above, so the bridge would wrap it again). ureq is a blocking client over `rustls` with the `ring` provider; one `spawn_blocking` call every 12 h is the right shape for it, and it adds nothing to the async stack.
- Dev-dependencies: `rsa = "0.9.10"` and `rand = "0.8.5"`, the versions `jsonwebtoken`'s `rust_crypto` already pulls (`cargo tree -i rsa` confirms one copy). Tests generate one 2048-bit key pair per test binary in a `std::sync::LazyLock`, derive the JWK (`n`, `e` base64url without padding, from `RsaPublicKey::n()` and `e()`) and the `EncodingKey` from `to_pkcs1_der()`. No key material is committed: a fixture private key in a public repository trips secret scanners and invites the question whether it is really a test key.

### Logging

- The request line gains `client=<client id>` for a valid token, `client=anonymous` when `required = false` let a request through without one, and no field when `[auth]` is absent. Never logged: the token, any header or claim other than the client id, the `WWW-Authenticate` value.
- Startup logs `auth on` with `issuer`, `audience`, `required` and `scope_prefix`, then `jwks loaded` with the key count and whether it came from the file or the provider.
- A refused token logs `token refused` with the error kind (`missing`, `malformed`, `unknown key`, `invalid`, `insufficient scope`) and nothing else.
- The existing never-logged test in `src/server.rs` is extended: a valid, an expired and a wrongly scoped request are sent, and the log must not contain the token, any of its three segments, the audience or `"scp"`, while it must contain `client=<id>`.

### Health and `--check-config`

- `/healthz` needs no token. When `[auth]` is configured the `200` body gains `"auth": {"jwks_keys": <n>, "jwks_age_s": <n|null>}`, and the check is degraded with reason `auth jwks unavailable` only when no keys are loaded at all, from neither the provider nor the file. Stale keys do not degrade: the provider's own rotation makes them fail requests, not health.
- `--check-config` prints `auth: off`, or `auth: on` followed by `issuer`, `audience`, `jwks_url`, `required` and `scope_prefix`. There are no secrets in the bridge config, and the command does not fetch the JWKS.

### Security model changes

- README "Security model": "The network is the access control" becomes "The network is the first access control"; a new bullet says an optional bearer token is a second gate, validated locally against the provider's JWKS with one scope per route, and that the bridge still binds a literal address and checks `Host` first.
- New CLAUDE.md rules: **only `src/auth.rs` parses and validates tokens**; tokens are never logged, nor any claim beyond the client id; `/healthz` stays token-free; every route in `router()` other than `/healthz` has a row in the scope table, and a test enforces it.
- The cask caveats do not change: auth is optional and the first-run steps are the same.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- Complete each task fully before the next; small, focused changes.
- **Every task includes new or updated tests** for the code it changes, covering success and error paths.
- **All tests and `mise run check` pass before the next task starts.** Where `mise` is unavailable, run the three cargo commands directly.
- No test talks to a real provider or the network. The JWKS comes from a scripted `JwksSource`; `HttpJwksSource` is tested against a `tokio::net::TcpListener` on `127.0.0.1:0` that writes hand-made HTTP responses, the pattern `src/server.rs` already uses for its serve tests. HTTP tests drive the real `Router` with `tower::ServiceExt::oneshot`.
- Update this plan when scope changes; mark deviations with ⚠️.

## Implementation Steps

### Task 1: Config

**Files:**
- Modify: `src/config.rs`, `src/main.rs`

- [x] `RawAuth` with `deny_unknown_fields`, `AuthConfig { issuer, audience, jwks_url: url::Url, required, scope_prefix }`, every rule from Design, new `ConfigError` variants naming the key
- [x] `--check-config` output from Design
- [x] tests: full table; defaults for `required` and `scope_prefix`; absent table is `None`; blank issuer; issuer with whitespace; blank audience; `jwks_url` relative, `ftp://`, blank; `scope_prefix` with a space, a quote, a backslash; empty `scope_prefix` accepted; unknown key; `describe` output with and without `[auth]`
- [x] run tests, `mise run check`

### Task 2: Token validation

**Files:**
- Create: `src/auth.rs` (the module is declared in `src/lib.rs` the moment it exists)
- Modify: `Cargo.toml`, `src/lib.rs`

- [x] `cargo add` the crates from Design; confirm `cargo tree -i rsa` shows one copy
- [x] `Claims`, `OneOrMany`, `Principal`, the `Validate` steps 1-6 over a given key set, `AuthError` kinds without values
- [x] the scope table, `scope_for(method, route) -> Option<&'static str>`, and the required scope as `scope_prefix` + name
- [x] a test key module under `#[cfg(test)]`: the `LazyLock` key pair, `jwk(kid)`, `mint(claims, kid)` and `mint_hs256(claims)` helpers
- [x] tests: valid token with `scp` array; valid with `scope` string; both at once unioned; missing token; `basic` scheme; two headers; empty token; wrong signature (a second key pair); unknown `kid`; no `kid`; expired beyond leeway and expired within 60 s; `nbf` 120 s ahead refused, 30 s ahead accepted; wrong `iss`; `aud` missing; `aud` without the audience; `aud` as a plain matching string; `alg: none`; HS256 signed with the public key bytes; `typ` missing; `typ` `JWT`; `typ` `application/AT+JWT` accepted; neither `sub` nor `client_id`; `client_id` preferred over `sub` as the principal; the error `Display` and `Debug` of every kind contain no claim value
- [x] run tests, `mise run check` (the three cargo commands run directly; `mise run check` fails in the sandbox with `bash: command not found`)
- ⚠️ `Claims` leaves out `exp`, `nbf`, `iat` and `jti`: `jsonwebtoken` checks `exp` and `nbf` itself, and fields that are deserialised but never read are `dead_code` warnings. Serde ignores them, so a non-integer `iat` or `jti` cannot fail a token.
- ⚠️ `KeySet::from_jwks` already does the JWK filtering from "Parsing" (RSA, `kid`, `alg` absent or `RS256`, `use` absent or `sig`), with a test. Task 3 adds the zero-keys-is-a-failure rule on top.
- ⚠️ `scope_for` returns the scope name, and `Validator::required_scope(method, route)` adds `scope_prefix`.
- ⚠️ `Cargo.toml` sets `[profile.dev.package.num-bigint-dig] opt-level = 3`: generating the two 2048-bit test keys drops from about 6 s to under 0.5 s.

### Task 3: JWKS cache

**Files:**
- Modify: `src/auth.rs` (or split into `src/auth/mod.rs`, `src/auth/jwks.rs`, `src/auth/fetch.rs` if it passes 1000 lines)

- [ ] `JwksSource` trait, `Jwks` with the startup load, the 12 h / 5 min refresh loop as `run_refresh(self: Arc<Self>)`, the unknown-`kid` refetch with the 60 s gate and the shared `Mutex`
- [ ] parsing and filtering of `JwkSet`; zero usable keys is a failed fetch
- [ ] persistence: write through temp file and rename, load at startup, `fetched_at`, `age()`, `key_count()`
- [ ] `HttpJwksSource` over `ureq` with the timeout, body cap, `http_status_as_error` and `RootCerts::PlatformVerifier`, on `spawn_blocking`
- [ ] tests with paused time and a scripted source: unknown `kid` triggers exactly one fetch then succeeds; a second unknown `kid` 30 s later does not fetch and fails, one at 61 s fetches; a failed startup fetch retries after 30 s and succeeds; the 12 h refresh fires; a failed refresh keeps the old keys and retries in 5 min; a body with no RSA keys keeps the old keys; a body over the cap is a failure; the file is written after a good fetch and loaded by a fresh `Jwks` with the same count and an age computed from `fetched_at`; a corrupt file is ignored; a key with `use: enc` and one with `alg: ES256` are dropped
- [ ] `HttpJwksSource` tests against a local listener: `200` body; `500`; a body over 64 KiB; a server that accepts and never answers, with the client timeout shortened for the test
- [ ] run tests, `mise run check`

### Task 4: Middleware, logging, health, wiring

**Files:**
- Modify: `src/server.rs`, `src/health.rs`, `src/main.rs`

- [ ] `App::new(config, runners, auth: Option<Arc<Authenticator>>)`; `main.rs` builds the `Authenticator` from `config.auth` with `HttpJwksSource` and the cache path beside the config file, spawns `run_refresh` next to the announcers and aborts it on shutdown
- [ ] `require_bearer` middleware in the position from Design, the three responses with their `WWW-Authenticate` values, the `/healthz` exemption, the `Principal` response extension
- [ ] `log_request` logs `client`; startup and refusal log lines from Design
- [ ] health: `auth` object in the `200` body, `DegradedReason::AuthJwksUnavailable` serialised as `auth jwks unavailable`, only when no keys are loaded
- [ ] the route-coverage test over `router()`
- [ ] HTTP tests through `oneshot`, with the scripted source preloaded: one valid request per scope name against a route in its group; a read token on a write route is `403` with the exact header; `required = true` without a token is `401 Bearer`; `required = false` without a token reaches the handler and logs `anonymous`; `required = false` with a bad token is still `401`; `/healthz` without a token is `200` in both modes and carries `auth`; `/healthz` with no keys loaded is `503 auth jwks unavailable`; an unknown path with a valid token is `404`, without one `401`; the `Host` check still answers `421` before the token is read; the body limit still answers `413` after a valid token; no `[auth]` table leaves every existing test untouched
- [ ] extend the never-logged test as described under Logging
- [ ] run tests, `mise run check`

### Task 5: Verify acceptance criteria

- [ ] every row of the response table, the scope table and the JWKS cache rules has a test
- [ ] every existing `server.rs` test still passes with `[auth]` absent
- [ ] `mise run check` passes

### Task 6: Docs and version

**Files:**
- Modify: `README.md`, `CLAUDE.md`, `Cargo.toml`

- [ ] README: the Security model bullets from Design; an "Authentication" section after "Config reference" with the `[auth]` example, how to request a token (`client_credentials` with `scope` and `audience`, with `auth.example.com` as the provider), the scope table and the `required = false` rollout; the config table rows; `401` and `403` rows in "Status codes"; `auth` and `auth jwks unavailable` in "Health check"; the `client` field in "Logs"; a troubleshooting entry for `auth jwks unavailable` and one for a token refused because `aud` is missing
- [ ] CLAUDE.md: the rules under Security model changes; mention `src/auth.rs` in the first paragraph
- [ ] version 0.7.0 in `Cargo.toml` and in the README health example
- [ ] run `mise run check`, `shellcheck scripts/*.sh`
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Manual, on the Mac, with a provider at hand:*

- Register a confidential client in the provider with the `client_credentials` grant, an audience for the bridge and the scopes `bridge:calendar.read`, `bridge:calendar.write`, `bridge:reminders.read`, `bridge:reminders.write`, `bridge:mail.read`, `bridge:mail.junk`; confirm its JWKS URL serves RS256 keys.
- Add `[auth]` with `required = false`, run `install`, and check the log: `auth on`, `jwks loaded` from the provider, then `client=<id>` on requests from a client that sends a token and `client=anonymous` on the rest. `jwks-cache.json` exists next to the config.
- Switch to `required = true`, run `install`, and confirm `curl` without a token gets `401` with `WWW-Authenticate: Bearer`, a token requested without `audience` gets `401`, a read-only token on `POST /v1/events` gets `403` naming `bridge:calendar.write`, and `/healthz` answers `200` with the `auth` object.
- Stop the provider briefly and restart the bridge: requests still pass on the persisted keys, and `/healthz` stays `200`.
- When every client sends tokens and `required = true` has run cleanly, a follow-up release removes `required` (a configured `[auth]` is then always enforced).
