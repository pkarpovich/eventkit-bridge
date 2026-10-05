# eventkit-bridge

A Rust daemon inside `EventKitBridge.app` that serves an HTTP API over the Mac's calendars and reminders by running two bundled, pinned CLIs: `ekctl` (v1.8.0) for calendars and `remindctl` (v0.3.8) for reminders. With an optional `[mail]` table it also reads Apple Mail's local store (`~/Library/Mail/V<n>`) directly, read-only. The bridge enforces one thing, the read/write policy over calendars and reminder lists, and the visibility rules over mail accounts and mailboxes. See `README.md` for the API, and `docs/plans/completed/2026-10-05-eventkit-bridge.md`, `docs/plans/completed/2026-10-05-reminders.md` and `docs/plans/completed/2026-10-05-mail.md` for the design.

## Gate

`mise run check` must pass: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. Where `mise` is unavailable, run the three cargo commands directly. `shellcheck scripts/*.sh` must be clean.

## Code conventions

- Apply the `rust-style` skill to every Rust file.
- `#![forbid(unsafe_code)]` at the crate root (`src/lib.rs`, `src/main.rs`). No unsafe, so no `libc`; use safe wrappers such as `rustix`.
- The library (`src/lib.rs`) holds every module; `src/main.rs` is only the CLI and daemon wiring. A new module is declared in `lib.rs` the moment it is created, or its tests never compile.
- The crate builds and tests on Linux as well as macOS. Nothing links an Apple framework; macOS is reached only through the `ekctl`, `remindctl` and `launchctl` subprocesses and by reading Mail's files, and no test runs any of them for real or reads a real `~/Library/Mail`. Mail tests build a fixture store with `src/mail/fixture.rs` from `fixtures/mail_schema.sql`. Tests use the fake script in `src/fake_ekctl.rs` (for both CLIs) and a recording fake for launchctl.
- No test may run the real `ekctl` or `remindctl`: from a terminal, TCC would attribute the calendar or reminders access to the terminal app.
- Lint findings are fixed in code. No `#[allow(...)]` except a narrow one on a single item with its reason stated in the PR. No blanket `#[allow(dead_code)]`; an item that exists only for tests is `#[cfg(test)]`.
- Tests live inline in a `#[cfg(test)] mod tests` block in the file they cover, and cover success and error paths.
- No comments. `///` doc comments on `pub` items only.
- No function with 4 or more parameters: pass a struct.
- Every subprocess call has a deadline and is killed when it expires (`kill_on_drop(true)`). Both runners (`src/ekctl.rs`, `src/remindctl.rs`) spawn through `src/subprocess.rs`, which owns the deadline, the stdout cap, the stderr tail and `CallOutcome`; never copy that into a runner.
- HTTP tests drive the real `axum::Router` with `tower::ServiceExt::oneshot`; no sockets.

## Rules that protect the security model

- **Only `src/ekctl.rs` builds `ekctl` argv.** Every option is `--name=value`, every event id goes after a literal `--`, and `ekctl` is run directly, never through a shell. No route ever passes client input to `ekctl` except through these typed builders.
- **Only `src/remindctl.rs` builds `remindctl` argv**, under the same rules: `--name=value` options, ids after a literal `--`, `--json --no-input` on every command, no shell.
- **Reminder and list ids must be full UUIDs** before they reach `remindctl` argv. `remindctl` reads an id argument as an "index or ID prefix", so a short number would address a row from its last listing. Anything else is `400`.
- Every `ekctl` and `remindctl` call goes through one shared lock (`StoreLock` in `src/subprocess.rs`), because both write to the same EventKit store. A write holds one session across its policy check (`show event` or `info`) and the write itself.
- Reminder writes go only to `write_lists`. `POST` names its list, and `PATCH` and `DELETE` run `info` first and refuse unless the reminder is in a write list.
- Writes go only to `write_calendars`. `POST` names its calendar, and `PATCH` and `DELETE` run `show event` first and refuse unless the event is in a write calendar. They also refuse a recurring event with `409`: `ekctl` resolves an id to the series' first occurrence and saves with `.thisEvent`, so the change would land on the wrong occurrence. Keep that refusal until `ekctl` can address one occurrence.
- The bridge never listens on an unspecified address.
- Every request's `Host` (and absolute-form authority) must be the listen IP or a configured `hosts` name; the check runs before any route, so DNS rebinding from a tailnet browser cannot reach a handler.
- **Never logged:** event titles, notes, locations, urls or attendees, and reminder titles or notes, from requests or from `ekctl`/`remindctl` output. The only exception is the startup listing (calendar id, title and source; reminder list id and title; place names and radii), which the user needs to fill in the config. A test in `src/server.rs` asserts this; keep it passing when adding routes or log lines.
- **Mail SQL is parameterised.** No query string is ever built from request input; filters are bound as named parameters, and id sets as one `rarray` parameter.
- **The Envelope Index is opened read-only and never `immutable`.** `SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only = 1` and a busy timeout, one connection per request on `spawn_blocking`, outside `StoreLock`. Mail writes the WAL concurrently, and `immutable=1` would hide it. No message data is ever written.
- **Message paths are confined to the mail root.** A `.emlx` path is built from the mailbox URL and the `ROWID` partition rule, canonicalised, and refused unless it stays under the canonical root. An error returned to a client names only the kind of failure, never the path.
- **Mail visibility:** only accounts in `[mail.accounts]`, never an excluded mailbox, never a `deleted = 1` row. An invisible message is `404`, the same as an unknown id.
- **Never logged from mail:** addresses, display names, subjects, summaries, bodies or attachment names. Request lines carry only a row count. The only exception is the startup account listing (account uuid, kind, `Accounts4.sqlite` type and description, counts, newest date). A test in `src/server.rs` asserts this; keep it passing.
- **Place addresses and coordinates are never logged or returned**, not in the startup listing, `--check-config`, `/v1/places`, a reminder's `location` or a `remindctl` error (a failed geocode names the address, so `Session::add` replaces it with the place name). A location trigger is reported only by its config place name and proximity. Types holding an address (`config::Address`, `RcLocationTrigger`) implement `Debug` by hand to print `<redacted>`; never derive it.

## Bundle id

The bundle id `dev.pkarpovich.eventkit-bridge` (and the signing identifiers `dev.pkarpovich.eventkit-bridge.ekctl` and `dev.pkarpovich.eventkit-bridge.remindctl`, the LaunchAgent label and `AssociatedBundleIdentifiers`) must never change. TCC keys the Calendars, Reminders and Full Disk Access grants to it, so changing it silently revokes every user's permission and makes them approve the prompt again (or re-add the app under Full Disk Access).

## Release

- `ekctl` is pinned in `scripts/fetch-ekctl.sh` and `remindctl` in `scripts/fetch-remindctl.sh` (version, URL, sha256). `fixtures/*.json` (`fixtures/remindctl_*.json` for `remindctl`) matches the output of that version, with personal values replaced, and the tests treat it as the tool's output contract. A bump updates the script, re-captures the fixtures from the app bundle (never from a terminal), and updates the version here and in the README.
- `fixtures/mail_schema.sql` is the subset of Mail's Envelope Index (data version `V10`) that the bridge reads, and `SCHEMA` in `src/mail/store.rs` lists the same tables and columns for the `mail schema changed` health check. When a macOS release changes the format, update both together with the README note in "Turning on mail". Capture the schema from a copy of the index, never with message data.
- The Homebrew cask is written inline by `.github/workflows/release.yml`. Its `caveats` repeat the README "First run" steps, including `100.64.0.1:8790`, and the optional Full Disk Access step from "Turning on mail"; change both together.
