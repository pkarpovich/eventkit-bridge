# eventkit-bridge

## Overview

`eventkit-bridge` is a small macOS daemon that exposes the Mac's calendars over an HTTP API bound to a private network address (a tailscale IP). Through EventKit it reads every calendar the Mac already syncs (iCloud, Google, Exchange), and it creates, updates and deletes events in exactly one calendar. Reminders come later through the same bridge.

It does not talk to EventKit directly. It runs a bundled, pinned copy of [`ekctl`](https://github.com/schappim/ekctl), a native Swift CLI over EventKit with JSON output, and turns the bridge's typed API into `ekctl` argv and `ekctl` JSON back into the bridge's own JSON.

```
HTTP client (on the tailnet) --HTTP--> eventkit-bridge (LaunchAgent, EventKitBridge.app) --exec--> ekctl --EventKit--> CalendarAgent
```

The bridge has no business logic. It enforces one thing, a **security policy**: reads touch only the configured calendars, writes touch only the one configured write calendar, and the bridge checks this itself before every write. A bug in a client can neither read a calendar outside the list nor change an event the user created.

The first consumer is the `calendar` MCP service in the `turtle-hub` repository, which runs on a home server and calls this API over tailscale. Nothing in this repository knows about it: the API is general and documented in the README.

Distribution follows the same pattern as the author's other Mac daemons:
- a Rust binary inside a signed, notarized `EventKitBridge.app`;
- a Homebrew cask in `pkarpovich/homebrew-apps`;
- the binary installs and manages its own LaunchAgent.

## Skills to invoke

- `rust-style` - every Rust file.

## Non-goals

- Reminders. Later they add the reminders entitlement, a list allowlist, and `/v1/reminders` routes.
- Calendar management: creating, renaming or deleting calendars.
- Writes outside the write calendar.
- Recurrence, invitations, attendee edits and alarms on created events. All-day event creation. Reading all-day events is supported.
- Any cache or database. Every request runs `ekctl` live; a week across every calendar takes about 0.2 s.
- OpenTelemetry export. The client traces its own calls; the bridge logs locally.
- An Intel build. Apple Silicon only.

## Rejected alternatives

- **A generic exec proxy (`POST /exec` with raw `ekctl` args)**, which an earlier draft planned. For a public tool, a typed API is the honest contract. The bridge builds every argv itself, so argument injection through `ekctl`'s flag parser cannot happen. A client never needs to know `ekctl` exists. If `ekctl` is ever replaced by a Swift helper, clients do not notice.
- **Calling EventKit from Rust through `objc2`.** EventKit's async access request and completion-handler APIs make this the largest piece of unsafe code in the project, and `ekctl` already exists and works. The Swift-helper fallback in Gate 0 is the escape hatch if `ekctl` stops being viable.
- **A loose binary instead of an app bundle.** TCC keys a grant to a bundle id plus a path that does not move. A loose binary under a Homebrew Cellar path is identified by that path, which changes with every version, so the grant is lost on every upgrade.
- **A bearer token.** Access control is the network: the bridge binds only to a literal tailscale IP, so only tailnet devices reach it - the same trust model as the other private services it talks to. A token would add a secret to rotate on both sides and protect nothing the tailnet does not already protect. The write policy is what limits damage, and it does not depend on who is calling.
- **A LaunchDaemon.** The Calendars prompt only appears in a logged-in GUI session, and EventKit data is per user.

## Context (verified on the target Mac, macOS 27, Apple Silicon)

- **`ekctl` v1.8.0:**
  - `list calendars` returns 27 event calendars plus reminder lists on the author's Mac.
  - `list events` across all of them for a week takes 0.18 s.
  - `free` exists only since v1.7.0; the Homebrew tap still ships v1.6.0.
  - Recurring occurrences are expanded and share the series `id`.
  - One work event carries 14 attendees and 2 KB of notes.
  - **Errors exit `0`** with `{"status":"error","error":"..."}` on stdout.
  - Options in `--name=value` form parse, values starting with `-` included. Positional ids after a literal `--` parse.
  - `--calendar` is required by `list events` and takes a comma-separated list of ids.
- **Release asset:** `https://github.com/schappim/ekctl/releases/download/v1.8.0/ekctl-v1.8.0.tar.gz`, sha256 `4e38314154e7df79c7e3eaa48f6330699ad2a6808ae033d9bd7510740787146a`. It contains one universal `ekctl` binary, ad-hoc signed.
- **`ekctl` license:** MIT, stated in its README; the repository has no LICENSE file. The app ships that statement and the upstream URL as `Contents/Resources/ekctl-LICENSE.txt`.
- **`ekctl`'s own entitlements file** declares `com.apple.security.personal-information.calendars` and `com.apple.security.personal-information.reminders`. Under the hardened runtime, which notarization requires, these are the entitlements that let a signed binary use EventKit.
- **TCC** grants a privacy permission to the *responsible* process. A child spawned by an app launched by launchd is attributed to that app, so `ekctl` inside `EventKitBridge.app` should be attributed to the bundle. Gate 0 verifies this before any code is written.
- **Signing:** `Developer ID Application: Pavel Karpovich (GGG699AY79)`. Release secrets (certificate p12, password, team id, App Store Connect key for notarization, tap token) live in the 1Password item `nhop release signing` and already feed two other repositories.
- **Toolchain:** Rust 1.99.0 is the newest stable (`mise ls-remote rust`); edition 2024.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- Complete each task fully before the next; small, focused changes.
- **Every task includes new or updated tests** for the code it changes, covering success and error paths.
- **All tests and the gate pass before the next task starts.**
- Update this plan when scope changes; mark deviations with ⚠️.

## Code-Quality Rules (gate for every task)

- `mise run check` passes: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- `#![forbid(unsafe_code)]` sits at the crate root. Nothing here needs unsafe.
- The crate builds and its tests pass on Linux as well as macOS: the implementation runs in a Linux container. Nothing links an Apple framework; macOS is reached only through the `ekctl` and `launchctl` subprocesses, and no test runs either of them for real. `codesign`, `plutil` and notarization run only in the release workflow on `macos-latest` and in Post-Completion.
- Lint findings are fixed in code. No `#[allow(...)]` except a narrow one on a single item with its reason stated in the PR. No blanket `#[allow(dead_code)]` on a module: an item that exists only for tests is `#[cfg(test)]`.
- Tests live inline in a `#[cfg(test)] mod tests` block in the file they cover. Every module is declared (`mod x;`) the moment it is created, or its tests never compile.
- No comments. `///` doc comments on `pub` items only.
- No function with 4+ parameters: pass a struct.
- Every subprocess call has a deadline and is killed when the deadline expires.

## Testing Strategy

- **Fake `ekctl`.** Tests write a small shell script into a temp dir (`tempfile` dev-dependency) and point the runner at it. It can:
  - print canned JSON from `fixtures/`;
  - write to stderr;
  - exit with a chosen code;
  - sleep past the timeout;
  - append its argv and start/end timestamps to a file, so tests assert exact arguments and that calls never overlap.
- **HTTP.** Tests drive the real `axum::Router` through `tower::ServiceExt::oneshot`, so no socket is needed.
- **No test touches EventKit.** A test that ran the real `ekctl` from a terminal would make TCC attribute the access to the terminal app. Real behaviour is verified in Gate 0 and in Post-Completion.

## Progress Tracking

- Mark completed items with `[x]` immediately.
- Add newly discovered tasks with ➕ prefix.
- Document issues or blockers with ⚠️ prefix.

## Solution Overview

### Command line (`argh`)

- `eventkit-bridge` with no subcommand runs the daemon. This is what the LaunchAgent starts.
- `eventkit-bridge install` writes and loads the LaunchAgent.
- `eventkit-bridge uninstall` unloads and removes the LaunchAgent. It keeps the config.
- `eventkit-bridge --check-config` validates the config file and exits `0` or `1` with the reason. It never runs `ekctl`: from a terminal, TCC would attribute the access to the terminal.
- `eventkit-bridge --version`.

### Config: `~/.config/eventkit-bridge/config.toml`

```toml
listen = "100.108.208.81:8790"
read_calendars = ["4F7D9489-A78F-4369-A951-213207DCFEE3"]
write_calendar = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10"
# ekctl = "/path/to/ekctl"   optional, for development outside the bundle
```

- **`listen`** (required) must be a literal `IP:port`. An unspecified address (`0.0.0.0`, `::`) is rejected: the bridge never listens on every interface.
- **`read_calendars`** (default empty) lists the calendar ids reads may touch.
- **`write_calendar`** (optional) is the only calendar writes may touch, and it is always readable. When it is absent, every write answers `403`.
- **`ekctl`** (optional) defaults to `ekctl` next to the running executable, which is `Contents/MacOS/ekctl` in the bundle.
- **First-run flow.** The user does not know calendar ids before the grant exists, so empty `read_calendars` and an absent `write_calendar` are valid:
  - on every start, after its first successful `ekctl list calendars`, the daemon logs one line per event calendar: `id`, `title`, `source`, and whether it is readable and writable under the current config;
  - `/healthz` reports `unconfigured` until `read_calendars` is non-empty.
- **Changing the config** means running `eventkit-bridge install` again, which reloads the agent. There is no hot reload.

### The `ekctl` runner

- `tokio::process::Command` runs `ekctl` directly, never through a shell, with `kill_on_drop(true)`, stdin null and stdout/stderr piped.
- **Serialization.** Every invocation goes through one `tokio::sync::Mutex`. EventKit does not like concurrent writers, and reads take 0.2 s. A write that first runs `show` holds the lock across both calls, so the policy decision and the write see the same event.
- **Timeout:** 20 s per invocation (a constant, injectable in tests), giving `504`.
- **Output limits:** stdout is capped at 8 MiB (`502 output too large`). Only the last 500 bytes of stderr are kept.
- **Result mapping:**
  - an `ekctl` that cannot start -> `502`;
  - a non-zero exit -> `502` with the stderr tail;
  - exit `0` with `{"status":"error","error":msg}` -> `404` when the command is `show event` and `msg` starts with `Event not found`, otherwise `502` with `ekctl: <msg>`;
  - stdout that is not the expected JSON shape -> `502 unexpected ekctl output`.
- **Argv** is built by the bridge only. Every option is `--name=value`, and every event id goes after a literal `--`:

```
list calendars
list events --calendar=<id,id,...> --from=<RFC3339> --to=<RFC3339>
show event -- <id>
free --calendar=<id,id,...> --duration=<n> --working-hours=<HH:MM-HH:MM|all> --weekdays=<spec> --buffer=<n> --limit=<n> [--from=<RFC3339>] [--to=<RFC3339>]
add event --calendar=<write id> --title=<t> --start=<RFC3339> --end=<RFC3339> [--location=<l>] [--notes=<n>] [--url=<u>]
update event [--title=<t>] [--start=<RFC3339>] [--end=<RFC3339>] [--location=<l>] [--notes=<n>] [--url=<u>] -- <id>
delete event -- <id>
```

### HTTP API

`axum` on the configured address, with no authentication: reachability is the tailnet. Request bodies are capped at 64 KiB. Every error body is `{"error":"<message>"}`.

**Shared types:**

```json
Calendar: {"id":"...","title":"Calendar","source":"iCloud","color":"#0088FF","writable":false}
Event: {"id":"...","title":"...","start":"2026-10-05T11:00:00+02:00","end":"2026-10-05T11:30:00+02:00",
        "all_day":false,"calendar":{"id":"...","title":"..."},"location":null,"url":null,"notes":null,
        "availability":"busy","recurring":true,
        "attendees":[{"name":"A Person","email":"a@example.com","role":"required","status":"accepted"}]}
Slot: {"start":"...","end":"...","duration_minutes":60,"weekday":"monday"}
```

- `start` and `end` keep the offset `ekctl` returned.
- `recurring` is `ekctl`'s `hasRecurrenceRules`. Occurrences of a recurring event share one `id`.
- A field `ekctl` omits or sets to `null` becomes `null`; `attendees` becomes `[]`.

**Routes:**

- **`GET /v1/calendars`** -> `{"calendars":[Calendar]}`.
  - Event calendars only (`type == "event"`), and only those that are readable.
  - `writable` is true for the write calendar alone.
- **`GET /v1/events?from=&to=[&calendar=<id>]...`** -> `{"events":[Event]}`, in `ekctl`'s order.
  - `from` and `to` are RFC3339, `from < to`, and the span is at most 62 days; otherwise `400`.
  - With no `calendar`, every readable calendar is used. A non-readable id gives `403 calendar not readable: <id>`.
- **`GET /v1/events/{id}`** -> `Event`.
  - `404` when the event is not found.
  - `403` when the event's calendar is not readable.
- **`GET /v1/free?duration=&working_hours=&weekdays=&buffer=&limit=[&from=&to=][&calendar=<id>]...`** -> `{"slots":[Slot],"searched_from":"...","searched_to":"..."}`. Defaults and bounds, `400` outside them:
  - `duration`: default 30, range 5-1440;
  - `working_hours`: default `09:00-17:00`, or `HH:MM-HH:MM` with start before end, or `all`;
  - `weekdays`: default `weekdays`. Otherwise `weekdays`, `weekends`, `all`, or a comma list of day names (`monday`/`mon` ...), where an item may be a range such as `mon-fri`;
  - `buffer`: default 0, range 0-240;
  - `limit`: default 20, range 1-100;
  - `from`/`to`: RFC3339 when given, otherwise not passed, so `ekctl`'s defaults apply (now to +7 days). When both are given, the span is at most 62 days;
  - `calendar`: same rules as `/v1/events`.
- **`POST /v1/events`**, body `{"title","start","end","location"?,"notes"?,"url"?}` -> `201 Event`, created in the write calendar.
  - `403 no write calendar configured` when `write_calendar` is absent.
- **`PATCH /v1/events/{id}`**, body with any non-empty subset of those fields -> `200 Event`.
  - The bridge runs `show event` first. It answers `403 event is not in the write calendar` unless the event's calendar is the write calendar, and validates the merged range (new or existing `start` against new or existing `end`) before `update event`.
- **`DELETE /v1/events/{id}`** -> `204`, with the same `show`-first check.
- **Input validation for writes** (`400`):
  - `title` must be non-blank, at most 500 characters;
  - `start` and `end` are RFC3339 with `end > start`, timed events only;
  - `location` at most 500 characters, `notes` at most 10 000;
  - `url` must parse as an absolute `http`/`https` URL, at most 2 000 characters;
  - no string may contain a control character other than `\n` and `\t`;
  - an empty `PATCH` body is rejected.
- **Path ids.** An event id is taken from one path segment, so a client percent-encodes it. Ids look like `46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076`. An id that is empty or contains a control character is `400`.

**`GET /healthz`** (no auth):
- `200 {"status":"ok","version":"<crate version>","calendars":<readable calendars that exist>}`.
- `503 {"status":"degraded","reason":...}` otherwise, where `reason` is one of:
  - `unconfigured` - `read_calendars` is empty;
  - `timeout`;
  - `ekctl failed` - this is how a revoked or never-granted TCC permission shows up;
  - `calendar missing` - a configured id no longer exists.
- The check runs `ekctl list calendars` at most once per 10 s. The result is cached for 10 s, and concurrent requests wait on the same check.

### Logging

- `tracing` to stdout. The LaunchAgent sends stdout and stderr to `~/Library/Logs/eventkit-bridge.log`, with no rotation (the README says to truncate it by hand).
- Per request, a log line carries method, route template (`/v1/events/{id}`, not the id), status, duration and, when `ekctl` ran, the subcommand and exit code.
- A policy refusal logs its reason.
- **Never logged:** titles, notes, locations, urls or attendees. The only exception is the startup calendar listing described under Config, which is what the user needs to fill in the ids.

### LaunchAgent and upgrades

- **`install`:**
  1. Resolves the running executable with symlinks resolved. The cask puts `eventkit-bridge` on `PATH` as a symlink into the app, and an agent pointing at the symlink would make TCC key the grant to the wrong path.
  2. Unloads any existing agent, waiting up to 5 s for `launchctl print gui/<uid>/<label>` to stop finding it.
  3. Writes `~/Library/LaunchAgents/dev.pkarpovich.eventkit-bridge.plist`.
  4. Runs `launchctl bootstrap gui/<uid> <plist>`.
  5. Prints the program path, and a warning when that path is not inside an `.app` bundle, because the grant would then not survive an upgrade.
- **Agent plist:**
  - `Label` is `dev.pkarpovich.eventkit-bridge`;
  - `ProgramArguments` is the resolved binary;
  - `RunAtLoad` is true;
  - `KeepAlive` is `{PathState: {<binary>: true}}`, so launchd restarts the daemon whenever it exits while the binary exists. This also retries a bind that fails at boot before tailscale is up;
  - `AssociatedBundleIdentifiers` is the bundle id;
  - `StandardOutPath` and `StandardErrorPath` are the log file.
- **Upgrade.** Every 2 s the daemon compares its executable's `(device, inode)` with the one it started from. When the file is swapped or removed (a cask upgrade replaces the app), it shuts down gracefully and exits `0`, and launchd starts the new binary. An upgrade needs no `install`.
- **Graceful shutdown** on SIGTERM and on swap: stop accepting connections and let in-flight requests finish, bounded at 25 s.

### Bundle, signing and release

- **`EventKitBridge.app`:**
  - `Contents/MacOS/eventkit-bridge`;
  - `Contents/MacOS/ekctl`;
  - `Contents/Resources/ekctl-LICENSE.txt`;
  - `Contents/Info.plist`, rendered from `Info.plist.template` with:
    - `CFBundleIdentifier=dev.pkarpovich.eventkit-bridge`;
    - `CFBundleName=EventKitBridge`;
    - `CFBundleExecutable=eventkit-bridge`;
    - `CFBundlePackageType=APPL`;
    - `CFBundleShortVersionString` and `CFBundleVersion` from `Cargo.toml`;
    - `LSUIElement=true`;
    - `LSMinimumSystemVersion=14.0`;
    - `NSCalendarsFullAccessUsageDescription` = "eventkit-bridge reads the calendars you allow and manages events in the one calendar you choose for it."
- **`entitlements.plist`:** `com.apple.security.personal-information.calendars = true`.
- **`scripts/fetch-ekctl.sh <out-dir>`:**
  - the asset URL and sha256 are constants in the script;
  - it downloads with `curl -fsSL`, verifies with `shasum -a 256 -c`, extracts and prints the binary path;
  - a mismatch exits non-zero before anything is extracted.
- **`scripts/bundle.sh <binary> <ekctl> <out-dir> [identity]`** assembles the app. With an identity it signs **inside out**:
  1. `ekctl` with `--options runtime --timestamp --entitlements entitlements.plist --identifier dev.pkarpovich.eventkit-bridge.ekctl`;
  2. the app with `--options runtime --timestamp --entitlements entitlements.plist --identifier dev.pkarpovich.eventkit-bridge`, and **no `--deep`**;
  3. then `codesign --verify --strict --deep --verbose=2`.

  `set -euo pipefail`; it refuses to run on a non-darwin host.
- **`scripts/build-signed.sh <team-id>`** builds the release binary, runs `fetch-ekctl.sh`, finds the `Developer ID Application (<team-id>)` identity in the login keychain and runs `bundle.sh` into `dist/`. It is for local builds and Gate 0.
- **`.github/workflows/ci.yml`:** on pull requests and on pushes to `main`, `macos-latest` runs `jdx/mise-action` and `Swatinem/rust-cache`, then `mise run check`, then `shellcheck scripts/*.sh`.
- **`.github/workflows/release.yml`**, on a `v*` tag:
  1. The tag must equal the `Cargo.toml` version.
  2. `mise run check`.
  3. `cargo build --release --target aarch64-apple-darwin`.
  4. `fetch-ekctl.sh`.
  5. Import the certificate (`apple-actions/import-codesign-certs`).
  6. `bundle.sh` with the Developer ID identity.
  7. Notarize the zipped app with `xcrun notarytool submit --wait`, printing the notary log on rejection, then `xcrun stapler staple`.
  8. `spctl --assess --type exec --verbose=2` must say `Notarized Developer ID`.
  9. Zip `EventKitBridge-arm64-<version>.zip` and write `checksums.txt`.
  10. Publish a GitHub Release.
  11. Write `Casks/eventkit-bridge.rb` into `pkarpovich/homebrew-apps` and push.

  Secrets: `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD`, `MACOS_TEAM_ID`, `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_CONTENT`, `HOMEBREW_TAP_TOKEN`.
- **Cask:**
  - `app "EventKitBridge.app"`;
  - `binary "#{appdir}/EventKitBridge.app/Contents/MacOS/eventkit-bridge"`;
  - `depends_on arch: :arm64`, `depends_on macos: :sonoma`;
  - `livecheck` on GitHub releases;
  - no `uninstall` launchctl stanza, because it would unload the agent on every upgrade and leave it down;
  - `zap` removes the config dir, the agent plist and the log;
  - `caveats` carry the setup steps from the README.

## Gate 0: TCC spike (manual, on the Mac, alongside the implementation)

The whole design rests on four facts:

1. EventKit access requested by `ekctl` that runs as a child of a signed app started by launchd is attributed to the app.
2. The prompt appears for it.
3. The grant survives a rebuild signed with the same identity.
4. Under the hardened runtime, the calendars entitlement is what makes it work.

Steps:

1. Build `EKBSpike.app`:
   - `Contents/MacOS/spike` is a shell script that runs `"$(dirname "$0")/ekctl" list calendars > /tmp/ekb-spike.json 2>&1`;
   - `Contents/MacOS/ekctl` is v1.8.0;
   - `Info.plist` uses bundle id `dev.pkarpovich.ekb-spike`, `LSUIElement`, and the usage description.

   Sign both inside out with the Developer ID, `--options runtime`, the entitlements file and `--timestamp`, then copy the app to `/Applications`.
2. Load a LaunchAgent with `RunAtLoad` pointing at `/Applications/EKBSpike.app/Contents/MacOS/spike`. Expect a Calendars prompt that names `EKBSpike`. Approve it and check that `/tmp/ekb-spike.json` lists calendars.
3. Rebuild, re-sign with the same identity, reload. Calendars must be listed with no new prompt.
4. Re-sign the app without the entitlement, `tccutil reset Calendar dev.pkarpovich.ekb-spike`, reload. Record whether access is denied; this confirms the entitlement is load-bearing.
5. Clean up: `tccutil reset Calendar dev.pkarpovich.ekb-spike`, `launchctl bootout`, remove the app and the plist.

If step 2 attributes the grant to anything other than the app, or no prompt appears, stop. The fallback is a small Swift helper inside the bundle that calls EventKit itself. Record the outcome here with ⚠️ and revise the plan before the first release.

The operator runs Gate 0; it is not an implementation task. Tasks 1-5 do not depend on its outcome. Only the bundle and entitlements in Task 6 would change if it fails.

## Implementation Steps

### Task 1: Crate scaffold, config and CLI

**Files:**
- Create: `Cargo.toml` (`eventkit-bridge` 0.1.0, edition 2024), `.mise.toml` (`rust = { version = "1.99", components = "rustfmt,clippy" }`, tasks `build`, `test`, `lint`, `fmt`, `check` as in the gate), `.gitignore`, `LICENSE` (MIT, Pavel Karpovich, 2026)
- Create: `src/main.rs`, `src/config.rs`

- [x] add dependencies with `cargo add` so current versions are resolved: `tokio` (rt-multi-thread, macros, process, signal, sync, time), `axum`, `serde` (derive), `serde_json`, `toml`, `argh`, `thiserror`, `tracing`, `tracing-subscriber`, `chrono` (std, clock), `url`; dev: `tempfile`, `tower` (util), `http-body-util`
- [x] `config.rs`: load from `$HOME/.config/eventkit-bridge/config.toml`, plus a path override used by tests. Apply every rule from Solution Overview: literal non-unspecified `listen`, write calendar always readable. The error type names the offending key
- [x] `main.rs`: `#![forbid(unsafe_code)]`, the `argh` CLI with `install`, `uninstall`, `--check-config`, `--version`; the daemon path is a stub that loads config and exits until Task 4
- [x] tests:
  - valid config;
  - each invalid case (missing listen, `0.0.0.0`, hostname instead of IP, malformed TOML);
  - write calendar implied readable;
  - absent write calendar
- [x] gate passes

### Task 2: `ekctl` runner and output parsing

**Files:**
- Create: `src/ekctl.rs`, `src/model.rs`
- Create: `fixtures/list_calendars.json`, `fixtures/list_events.json`, `fixtures/show_event.json`, `fixtures/free.json`, `fixtures/add_event.json`, `fixtures/delete_event.json`, `fixtures/error.json` (content under Technical Details)

- [x] `model.rs`: serde types for the `ekctl` shapes (tolerant of unknown fields, a missing `url` and `null` values) and the bridge's own `Calendar`, `Event`, `Attendee`, `Slot`; conversions from one to the other
- [x] `ekctl.rs`: the `Runner` (path, timeout, mutex), with the exec, timeout, stdout cap, stderr tail and the result mapping from Solution Overview, returning a typed error the HTTP layer maps to a status
- [x] typed argv builders for every command in Solution Overview; nothing outside this module builds argv
- [x] tests with the fake `ekctl`:
  - each fixture parses and converts;
  - error envelope with exit 0;
  - `Event not found` on `show` vs other errors;
  - non-zero exit with stderr tail;
  - timeout kills the child (the fake's end marker is never written);
  - binary missing;
  - stdout over the cap;
  - unexpected JSON;
  - exact argv for each builder, including a title starting with `-` and an id after `--`;
  - two concurrent calls never overlap
- [x] gate passes
- ⚠️ the crate is split into `src/lib.rs` (`#![forbid(unsafe_code)]`, `pub mod config; pub mod ekctl; pub mod model;`) and `src/main.rs` (the CLI, using the library). Without the split, items not yet reachable from `main` until Task 4 fail `clippy -D warnings` as dead code, and a blanket `allow(dead_code)` is forbidden. Later modules go into `lib.rs` too
- ⚠️ `tokio` also needs the `io-util` feature, for reading `ekctl`'s pipes
- ⚠️ timestamps the bridge passes to `ekctl` are formatted with whole seconds (`2026-10-05T11:00:00+02:00`); Task 4 should reject or accept fractional seconds knowingly

### Task 3: Policy

**Files:**
- Create: `src/policy.rs`

- [x] `Policy` from config: `readable(id)`, `writable(id)`, `filter_calendars` (event calendars only, readable only, `writable` flag set), `require_readable(ids)`, `default_read_set()`
- [x] the write guard: given an event id, run `show` through the runner and allow only when the event's calendar is the write calendar. It returns the shown event so `PATCH` can merge the range, and it runs inside the same runner lock as the write that follows
- [x] tests:
  - filtering on the `list calendars` fixture (reminder lists dropped, non-readable dropped, writable flag);
  - non-readable id refused;
  - empty request set means all readable;
  - write guard allows the write calendar, refuses a user calendar, and passes through not-found
- [x] gate passes
- ⚠️ the fake `ekctl` used by tests moved from `ekctl.rs` into `src/fake_ekctl.rs` (`#[cfg(test)]`) so `policy.rs` and the Task 4 HTTP tests share it. `Policy` also exposes `write_calendar()`, which gives `403 no write calendar configured` for `POST`

### Task 4: HTTP API and health

**Files:**
- Create: `src/server.rs`, `src/health.rs`
- Modify: `src/main.rs`

- [ ] router with every route, the 64 KiB body limit, query and body validation exactly as in Solution Overview, and the status mapping from runner and policy errors to `{"error":...}` bodies
- [ ] `health.rs`: the cached and collapsed check with the four degraded reasons
- [ ] `main.rs`: the daemon:
  - tracing to stdout;
  - bind `listen`; on failure, exit non-zero with the reason and let launchd retry;
  - the startup calendar listing;
  - serve until SIGTERM or swap (Task 5 wires swap), with the 25 s graceful bound
- [ ] request logging per Solution Overview. A test captures the log output (a `tracing` subscriber writing to a buffer) and asserts that a request with a title, notes, location, url and attendees leaves none of them in the log
- [ ] tests through `oneshot`:
  - each route's success;
  - each `400` validation rule;
  - `403` for non-readable calendars, writes without a write calendar, and update/delete of a user event (the fake records that no `update`/`delete` ran);
  - `404` from `show`;
  - `PATCH` with only `start` validated against the existing `end`;
  - empty `PATCH`;
  - `502`/`504` mapping;
  - healthz ok, unconfigured, timeout, ekctl failed and calendar missing;
  - cache reuse (the fake counts calls)
- [ ] gate passes

### Task 5: LaunchAgent install and upgrade detection

**Files:**
- Create: `src/service.rs`, `src/executable.rs`
- Modify: `src/main.rs`

- [ ] `service.rs`: `install` and `uninstall` per Solution Overview. Plist rendering is a pure function from `(binary, log path)` to the plist string, and launchctl calls go through a small trait so tests do not run launchctl
- [ ] `executable.rs`: `(device, inode)` identity of the canonical executable; a future that resolves when the file is swapped or removed (2 s poll); wired into the daemon's shutdown signal
- [ ] tests:
  - rendered plist is valid XML containing the canonical path, `KeepAlive.PathState`, `AssociatedBundleIdentifiers` and log paths;
  - housing detection inside and outside `.app`;
  - swap detection on a temp file replaced by rename and on removal;
  - install sequence order (unload, wait, write, bootstrap) against a recording fake
- [ ] gate passes

### Task 6: Bundle, release and cask

**Files:**
- Create: `Info.plist.template`, `entitlements.plist`, `ekctl-LICENSE.txt`, `scripts/fetch-ekctl.sh`, `scripts/bundle.sh`, `scripts/build-signed.sh`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`

- [ ] the three scripts per Solution Overview; `shellcheck scripts/*.sh` clean
- [ ] `ekctl-LICENSE.txt`: "ekctl by schappim, MIT License, https://github.com/schappim/ekctl", with the MIT text
- [ ] both workflows per Solution Overview; the cask `caveats` text matches the README setup section
- [ ] both plists parse: `python3 -c 'import plistlib,sys; plistlib.loads(sys.stdin.read().replace("__VERSION__","0.1.0").encode())' < Info.plist.template` and the same for `entitlements.plist`
- [ ] gate passes (no Rust changes, but CI runs it)

### Task 7: README

**Files:**
- Create: `README.md`, `CLAUDE.md`

- [ ] `README.md` (public-facing, full sentences):
  - what it is and the security model;
  - install with `brew install --cask pkarpovich/apps/eventkit-bridge`;
  - first run: write the config with `listen`, `eventkit-bridge --check-config`, `eventkit-bridge install`, approve the prompt, read the calendar ids from the log, fill `read_calendars` and `write_calendar`, `install` again;
  - the full HTTP API with examples;
  - healthz;
  - upgrade (nothing to do);
  - logs;
  - troubleshooting (`ekctl failed` -> check the grant in System Settings, `tccutil reset Calendar dev.pkarpovich.eventkit-bridge`; alive only after login after a reboot with FileVault);
  - releasing (tag flow, the seven secrets and where they come from);
  - credits to `ekctl`
- [ ] `CLAUDE.md`: the code conventions from Code-Quality Rules; the rule that only `ekctl.rs` builds argv; the never-logged fields; that the bundle id must never change because it keys the TCC grant
- [ ] gate passes

### Task 8: Verify acceptance criteria

- [ ] every route, rule and status in Solution Overview has a test
- [ ] `mise run check` green, `shellcheck` clean

### Task 9: [Final] Documentation

- [ ] README matches the code
- [ ] move this plan to `docs/plans/completed/`

## Technical Details

### `ekctl` v1.8.0 output fixtures

Real shapes with personal values replaced.

`list calendars`:
```json
{"calendars":[{"allowsModifications":true,"color":"#0088FF","id":"4F7D9489-A78F-4369-A951-213207DCFEE3","source":"work@example.com","title":"Calendar","type":"event"},{"allowsModifications":true,"color":"#34C759","id":"8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10","source":"iCloud","title":"Agent","type":"event"},{"allowsModifications":true,"color":"#007AFF","id":"2F8BCC68-AD77-B8A4-9218-37BF6271D47D","source":"iCloud","title":"Reminders","type":"reminder"}]}
```

`list events`:
```json
{"count":1,"status":"success","events":[{"alarms":[],"allDay":false,"attendees":[{"email":"a@example.com","name":"A Person","role":"required","status":"accepted"}],"availability":"busy","calendar":{"id":"4F7D9489-A78F-4369-A951-213207DCFEE3","title":"Calendar"},"endDate":"2026-10-05T11:30:00+02:00","hasAlarms":false,"hasRecurrenceRules":true,"id":"46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076","location":"Teams","notes":"long text","startDate":"2026-10-05T11:00:00+02:00","title":"Standup","travelTimeMinutes":null}]}
```
`url` is present only on some events.

`show event`: `{"status":"success","event":{...same fields as one list entry...}}`

`free`:
```json
{"busyEventCount":8,"count":1,"minimumDurationMinutes":60,"searchedFrom":"2026-10-04T21:40:21+02:00","searchedTo":"2026-10-07T21:40:21+02:00","slots":[{"date":"2026-10-05","durationMinutes":60,"endDate":"2026-10-05T10:00:00+02:00","startDate":"2026-10-05T09:00:00+02:00","weekday":"monday"}],"status":"success","weekdays":"monday,tuesday,wednesday,thursday,friday","workingHours":"09:00-17:00"}
```

`add event` / `update event` (shape from the `ekctl` README, confirmed in Post-Completion):
```json
{"status":"success","message":"Event created successfully","event":{"id":"NEW123:EVENT456","title":"Lunch","calendar":{"id":"8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10","title":"Agent"},"startDate":"2026-02-10T12:30:00Z","endDate":"2026-02-10T13:30:00Z","location":null,"notes":null,"allDay":false}}
```
The write result lacks fields the full event has, so `POST` and `PATCH` run `show event` on the returned id before answering, and the response is always a complete `Event`.

`delete event`: `{"status":"success","message":"Event 'X' deleted successfully","deletedEventID":"ABC123:DEF456"}`

Error, exit code 0: `{"status":"error","error":"Event not found with ID: nonexistent-id"}`

## Post-Completion

*On the Mac.*

- Before tagging: `scripts/build-signed.sh GGG699AY79` produces a bundle that passes `codesign --verify --strict --deep`, and `codesign -d --entitlements - dist/EventKitBridge.app` shows the calendars entitlement.
- Set the seven release secrets (README, Releasing), tag `v0.1.0`, and watch the release.

- Create the write calendar the client will use (for turtle-hub, an iCloud calendar named "Agent") and decide which calendars are readable.
- `brew install --cask pkarpovich/apps/eventkit-bridge`, then follow the README first-run flow with `listen = "100.108.208.81:8790"`.
- `curl http://100.108.208.81:8790/healthz` reports ok.
- One create, update and delete in the write calendar through the API; check the `add`/`update` fixtures against the real output and correct them if they differ. Watch the event appear on the iPhone.
- Try `PATCH` and `DELETE` on a user event and confirm `403`.
- `brew upgrade --cask eventkit-bridge` across a version bump: the daemon restarts by itself and the grant holds with no prompt.
- Add a Gatus probe on `http://100.108.208.81:8790/healthz` (`[BODY].status == ok`) in home-environment.
