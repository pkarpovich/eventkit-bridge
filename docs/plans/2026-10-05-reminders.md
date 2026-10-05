# Reminders in eventkit-bridge

## Overview

eventkit-bridge 0.3.0 adds Apple Reminders next to calendars. A client can:
- read the configured reminder lists;
- add, change, complete and delete reminders in the configured write lists, with a due date or date-time, a repeat rule and a priority;
- attach a location trigger ("when I arrive at" / "when I leave") to a new reminder, picked by name from places declared in the config.

The first consumers are an agent's shopping list and its "remind me tomorrow at 9" requests, through the `reminders` service in turtle-hub, but the API is general.

Reminders are served by a second bundled CLI, [`remindctl`](https://github.com/openclaw/remindctl) (MIT), pinned like `ekctl`. `ekctl` keeps serving calendars. The security model does not change: the bridge builds every argv itself, refuses lists outside the config, and checks a reminder's list before every write.

## Skills to invoke

- `rust-style` - every Rust file.

## Non-goals

- Reminder sections, tags, subtasks, smart lists, the Groceries list type and the Urgent toggle. EventKit has no public API for them, and `remindctl` deliberately uses public EventKit only.
- Alarms separate from the due time, URLs, and repeat rules other than the plain frequencies below.
- Adding a location trigger to an existing reminder. `remindctl edit` has no `--location`, so a trigger is set when the reminder is created.
- Free-form location addresses from clients. A location trigger names a place from the config; addresses never cross the API in either direction.
- Creating, renaming or deleting reminder lists.
- Moving a reminder between lists.

## Rejected alternatives

- **`ekctl` for reminders.** It has no location triggers.
- **`viticci/remctl`.** It writes sections and Groceries lists through Apple's private ReminderKit framework, installs its own Python under `/Library` as root, runs a background helper app and needs Full Disk Access. Too heavy and too fragile to bundle into a bridge whose job is to stay small.
- **`BRO3886/rem`.** Location triggers take raw coordinates only, and the project is less active than `remindctl`.
- **Free-form addresses through the API.** CoreLocation geocodes street addresses but not store names (verified: a store name fails with `kCLErrorDomain error 8`, its street address resolves). Named places keep geocoding predictable, and keep home and shop addresses out of client logs and agent context.

## Context (verified on the target Mac with remindctl 0.3.8)

- **Release asset:** `https://github.com/openclaw/remindctl/releases/download/v0.3.8/remindctl-macos.zip`, sha256 `b1a7ff303bea4fba7dd3c63416e8203ecef51769e87dcb464e8bbf9822363d52`. It contains one universal `remindctl` binary, signed by `Developer ID Application: OpenClaw Foundation (FWJYW4S8P8)` with the hardened runtime.
- **Argument parsing:**
  - options accept `--name=value`, and values starting with `-` or `--` are kept verbatim (a title `-dash-test` and notes `--json` round-trip);
  - a literal `--` ends the options, and positional ids follow it;
  - **an id argument is an "index or ID prefix"**: a short number is read as a row index from the last listing. The bridge must only ever pass a full UUID.
- **Commands used, all with `--json --no-input`:**
  - `list` -> array of `{id, title, reminderCount, overdueCount}` (counts are incomplete reminders);
  - `show <open|completed|all> --list-id=<id>` -> array of reminders;
  - `info -- <id>` -> one reminder;
  - `add --title=<t> --list-id=<id> [--notes=<n>] [--due=<due>] [--repeat=<freq>] [--priority=<p>] [--location=<address> --radius=<m> [--leaving]]` -> the created reminder;
  - `edit [--title=<t>] [--notes=<n>] [--due=<due>|--clear-due] [--repeat=<freq>|--no-repeat] [--priority=<p>] [--complete|--incomplete] -- <id>` -> the reminder;
  - `delete --force -- <id>` -> `{"deleted": 1}`;
  - `status` -> `{"authorized": true, "status": "full-access"}`.
- **Reminder JSON keys:** `id`, `title`, `notes` (absent when empty), `isCompleted`, `completionDate` (when completed), `listID`, `listName`, `priority` (`none|low|medium|high`), `dueDate` (UTC), `dueDateIsAllDay`, `alarmDate`, `recurrenceRule` (`{"frequency","interval"}`), `creationDate`, `lastModifiedDate`, and `locationTrigger` when set: `{"address", "latitude", "longitude", "proximity": "arriving"|"leaving", "radius"}`.
- **Due dates (verified):** `--due` accepts an RFC 3339 date-time with any offset, and then also sets `alarmDate` to the same instant, so the iPhone notifies at that time. A bare `YYYY-MM-DD` makes an all-day reminder with no alarm. `--repeat=weekly` is stored as `{"frequency":"weekly","interval":1}`; `--priority=high` round-trips.
- **Errors:** exit code `1` with a one-line message on stderr and nothing on stdout, for example `Reminder not found: "<id>".` and `List not found: "<id>".`. This differs from `ekctl`, which reports errors as JSON on stdout.
- **Geocoding** happens inside `remindctl add` through CoreLocation and needs network access. A store name does not resolve; a street address does.
- **TCC:** Reminders is a separate privacy service from Calendars. The app needs `com.apple.security.personal-information.reminders` and `NSRemindersFullAccessUsageDescription`, and the first reminders call after the upgrade shows one prompt. Gate 0 of the first plan proved that a child binary inside the signed bundle is attributed to the app for Calendars; the same mechanism applies, and Post-Completion confirms it for Reminders.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- Complete each task fully before the next; small, focused changes.
- **Every task includes new or updated tests** for the code it changes, covering success and error paths.
- **All tests and the gate pass before the next task starts.**
- Update this plan when scope changes; mark deviations with ⚠️.

## Code-Quality Rules (gate for every task)

The rules in `CLAUDE.md` apply unchanged:
- `mise run check` passes;
- `#![forbid(unsafe_code)]`;
- no `#[allow]` except a narrow one with a stated reason;
- tests inline;
- every module declared;
- no comments, `///` on `pub` items only;
- no function with 4+ parameters (clippy enforces 3);
- every subprocess has a deadline;
- the crate builds and tests on Linux.

## Testing Strategy

- **The fake runner is reused:** a shell script in a temp dir that prints canned JSON from `fixtures/remindctl_*.json`, writes stderr, exits with a chosen code and records its argv.
- **HTTP tests** go through `oneshot` against the real router.
- **No test runs the real `remindctl`.**

## Solution Overview

### Config

New optional keys:

```toml
read_lists = ["<list id>"]
write_lists = ["<list id>"]

[places]
shop = { address = "<street address>", radius = 150 }
home = { address = "<street address>" }
# remindctl = "/path/to/remindctl"   optional, for development outside the bundle
```

- `write_lists` are always readable.
- A place name matches `^[a-z0-9][a-z0-9-]{0,39}$`.
- `address` must be non-empty. `radius` is in meters, 50-2000, default 100.
- **Addresses are never logged**, not even in the startup listing.
- **Startup listing.** After the calendar listing, the daemon logs one line per reminder list: `id`, `title`, and whether it is readable and writable. This is how the user finds the list ids. It also logs place names, without addresses.
- **`--check-config`** prints the readable and writable lists and the place names.

### Runner

- A second runner executes `remindctl`. It has the same timeout, output cap and kill-on-drop behaviour as the `ekctl` runner.
- **Both runners share one lock**, because both binaries write to the same EventKit store.
- **Result mapping for `remindctl`:**
  - exit `0` -> parse stdout;
  - exit non-zero with stderr starting `Reminder not found` -> `404`;
  - `List not found` -> `502`, because the list was checked against the config first, so this is a stale config;
  - anything else -> `502` with the stderr tail;
  - stdout that is not the expected JSON -> `502 unexpected remindctl output`.
- **Argv is built in one module only.** Options use `--name=value`, ids go after `--`, and every command gets `--json --no-input`.
- **Ids.** A reminder id or list id must match a full UUID (`^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$`) before it reaches argv. Anything else is `400`, so an index can never be passed.

### HTTP API

Same rules as the calendar routes: `Host` allowlist (`421`), JSON content type on writes (`415`), 64 KiB bodies, `{"error": ...}` errors.

**Shared types:**

```json
List: {"id":"...","title":"Shopping","open":12,"writable":true}
Reminder: {"id":"...","title":"Milk","notes":null,"completed":false,"completed_at":null,
           "due":"2026-10-06T09:00:00+02:00","all_day":false,"repeat":"weekly","priority":"none",
           "list":{"id":"...","title":"Shopping"},
           "location":{"place":"shop","proximity":"arriving"}}
```

- `due` is `null` without a due date. For a timed reminder it is RFC 3339 in the Mac's local zone. For an all-day reminder it is `YYYY-MM-DD` with `all_day: true`.
- `repeat` is `null` or one of `daily`, `weekly`, `biweekly`, `monthly`, `yearly`. A rule `remindctl` reports with any other frequency or interval is shown as `custom` and left alone.
- `location` is `null` without a trigger.
- `place` is the config name whose address equals the trigger's `address`. A trigger that matches no configured place (created on a phone, say) reports `"place": null` with its `proximity` only. **Coordinates and addresses are never returned.**

**Routes:**

- **`GET /v1/lists`** -> `{"lists":[List]}`. Readable lists only.
- **`GET /v1/places`** -> `{"places":[{"name":"shop","radius":150}]}`. Names and radii only.
- **`GET /v1/reminders?status=open|completed|all[&list=<id>]...`**
  - `status` defaults to `open`; with no `list`, every readable list is used.
  - A non-readable list is `403`.
  - Answers `{"reminders":[Reminder]}` in `remindctl`'s order.
- **`GET /v1/reminders/{id}`** -> `Reminder`.
  - `404` when the reminder does not exist.
  - `403` when its list is not readable.
- **`POST /v1/reminders`**, body `{"list","title","notes"?,"due"?,"repeat"?,"priority"?,"place"?,"proximity"?}` -> `201 Reminder`.
  - `list` must be a write list.
  - `title` is non-blank, at most 500 characters. `notes` is at most 10 000 characters.
  - No string may contain a control character other than `\n` and `\t`.
  - `due` is an RFC 3339 date-time with an offset, whole seconds (timed, notifies at that time), or `YYYY-MM-DD` (all-day, no notification).
  - `repeat` is one of `daily`, `weekly`, `biweekly`, `monthly`, `yearly`, and needs `due`.
  - `priority` is `none`, `low`, `medium` or `high`.
  - `place` must be a configured name.
  - `proximity` is `arriving` (default) or `leaving`, and is only allowed with `place`.
  - Unknown fields are `400`.
- **`PATCH /v1/reminders/{id}`**, body with a non-empty subset of `{"title","notes","due","repeat","priority","completed"}` -> `200 Reminder`. `due: null` clears the due date (`--clear-due`), `repeat: null` removes the rule (`--no-repeat`), and the other fields follow the `POST` rules.
  - The bridge runs `info` first and answers `403 reminder is not in a writable list` unless the reminder's list is a write list.
  - `completed: true|false` maps to `--complete|--incomplete`.
- **`DELETE /v1/reminders/{id}`** -> `204`, with the same `info`-first check.
- **Holding the lock.** A write and its `info` check run in one runner session.

### Health

- When `read_lists` or `write_lists` is non-empty, `/healthz` also runs `remindctl status`, cached together with the calendar check.
- `authorized: false` reports `degraded` with reason `reminders access missing`.
- The `200` body gains `"lists": <readable lists that exist>`.
- With no lists configured, health is unchanged and `remindctl` is never run.

### Bundle and release

- `scripts/fetch-remindctl.sh` mirrors `fetch-ekctl.sh`: the URL and sha256 are constants, and a mismatch exits before extraction.
- `bundle.sh` takes the `remindctl` path as a new argument.
  - It installs it as `Contents/MacOS/remindctl` and adds `Contents/Resources/remindctl-LICENSE.txt`.
  - It signs it inside out before the app, with our Developer ID, `--options runtime`, the entitlements file and `--identifier dev.pkarpovich.eventkit-bridge.remindctl`. This replaces the OpenClaw signature.
- `entitlements.plist` gains `com.apple.security.personal-information.reminders = true`.
- `Info.plist.template` gains `NSRemindersFullAccessUsageDescription`: "eventkit-bridge reads the reminder lists you allow and manages reminders in the lists you choose for it."
- `release.yml` and `build-signed.sh` call `fetch-remindctl.sh`.
- The cask caveats and README mention the second permission prompt.
- `Cargo.toml` goes to `0.3.0`.

## Implementation Steps

### Task 1: Config for lists and places

**Files:**
- Modify: `src/config.rs`, `src/main.rs` (`--check-config` output)

- [x] `read_lists`, `write_lists`, `[places]` and `remindctl` keys with the rules above; errors name the key
- [x] `readable_lists()` = read plus write lists without duplicates; `remindctl_path(executable)` like `ekctl_path`
- [x] tests:
  - valid config;
  - invalid list id;
  - invalid place name;
  - empty address;
  - radius out of range;
  - default radius;
  - write list implied readable;
  - `--check-config` lists places by name without addresses
- [x] gate passes

### Task 2: remindctl runner, argv and parsing

**Files:**
- Create: `src/remindctl.rs`, `src/reminders_model.rs`
- Create: `fixtures/remindctl_list.json`, `remindctl_show.json`, `remindctl_info.json`, `remindctl_info_location.json`, `remindctl_add.json`, `remindctl_edit.json`, `remindctl_delete.json`, `remindctl_status.json` (shapes from Context, with placeholder ids, titles and coordinates)
- Modify: `src/ekctl.rs` (share the lock)
- ⚠️ Create: `src/subprocess.rs`. The spawn, deadline, output cap and stderr tail moved here from `ekctl.rs`, together with the `StoreLock` both runners share, so `remindctl.rs` reuses them instead of copying them.

- [x] a runner for `remindctl` sharing the `ekctl` runner's lock; the error mapping from Solution Overview
- [x] argv builders for every command, plus the UUID check
- [x] serde types and conversion to `List`/`Reminder`: `due` converted from UTC to the local zone, or to a date for all-day; `repeat` mapped or `custom`. Place resolution by exact address match against the config; addresses and coordinates dropped
- [x] tests:
  - each fixture parses;
  - exact argv per builder, including a `-`-leading title and `--` before ids;
  - an index-like id (`1`, a prefix) rejected before exec;
  - `Reminder not found` -> not found;
  - other stderr -> upstream error with the tail;
  - timeout;
  - a reminder call and a calendar call never overlap (the fake records start and end)
- [x] gate passes

### Task 3: Policy for lists

**Files:**
- Modify: `src/policy.rs`, `src/server.rs` (the new refusals map to `403`)
- ⚠️ Also added `require_writable_list` (for `POST`) and `require_reminder_readable` (for `GET /v1/reminders/{id}`), so Task 4's routes take every list decision from `policy.rs`. The guard returns `ReminderGuardError` (`Denied` or `Remindctl`).

- [x] `readable_list`, `writable_list`, `filter_lists`, `require_readable_lists`, and `guard_reminder_write` (`info` first, refuse a non-writable list)
- [x] tests: filtering, refusals, guard allows and refuses, not-found passes through
- [x] gate passes

### Task 4: Reminder routes and health

**Files:**
- Modify: `src/server.rs`, `src/request.rs`, `src/health.rs`, `src/main.rs`
- ⚠️ Also modified: `src/subprocess.rs` (`CallOutcome` moved here so both runners' call logs share it), `src/remindctl.rs` (its own task-local `CallLog`), `src/policy.rs` (`any_readable_list`, so `GET /v1/lists` answers `[]` without running `remindctl` when no lists are configured).
- ⚠️ `App::new` takes `Runners { calendars, reminders }`, and `HealthCheck::check` takes a `Probe` struct (clippy's argument limit). Health also runs `remindctl list` to count the readable lists that exist, and reports `remindctl failed` (or `timeout`) when `remindctl` itself fails.
- ⚠️ `PATCH` also refuses with `400 `repeat` needs `due`` when the change would leave a repeat rule without a due date, checked against the `info` result.
- ⚠️ The startup listing of reminder lists runs even with no lists configured, because it is how the user finds the list ids.

- [x] the six routes with validation and status mapping; the write guard in one session; startup listing of lists and place names
- [x] health: `remindctl status` when lists are configured; the `lists` count; the new degraded reason
- [x] request logging carries `remindctl="<command>=<exit>"`. A test asserts that titles, notes, place addresses and coordinates never reach the log
- [x] tests through `oneshot`:
  - every route's success;
  - every `400` rule, including `repeat` without `due`, a fractional-second `due` and an unknown priority;
  - `due` as a date-time and as a date map to the right argv, and `null` maps to `--clear-due`;
  - `403` for non-readable and non-writable lists (no write ran);
  - `404`;
  - an unknown `place`;
  - `proximity` without `place`;
  - health ok and `reminders access missing`;
  - no `remindctl` call when no lists are configured
- [x] gate passes

### Task 5: Bundle, release and docs

**Files:**
- Create: `scripts/fetch-remindctl.sh`, `remindctl-LICENSE.txt` (the MIT text from the `remindctl` repository)
- Modify: `scripts/bundle.sh`, `scripts/build-signed.sh`, `.github/workflows/release.yml`, `entitlements.plist`, `Info.plist.template`, `README.md`, `CLAUDE.md`, `Cargo.toml`

- [x] the bundle and release changes from Solution Overview; `shellcheck` clean; both plists parse
- [x] README:
  - the reminder routes;
  - lists and places in the config;
  - why places are named rather than free-form;
  - the second permission prompt;
  - that sections and the Groceries list type are not available
- [x] CLAUDE.md:
  - only `remindctl.rs` builds `remindctl` argv;
  - ids must be full UUIDs because `remindctl` reads short numbers as row indexes;
  - addresses and coordinates are never logged or returned
- [x] version 0.3.0
- [x] gate passes

### Task 6: Verify acceptance criteria

- [x] every route, rule and status above has a test
- [x] `mise run check` green, `shellcheck` clean

### Task 7: [Final] Documentation

- [ ] README matches the code
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*On the Mac, after v0.3.0 is released and `brew upgrade --cask eventkit-bridge` ran.*

- **First run.** The daemon restarts by itself after the upgrade. The first reminders call shows a Reminders prompt naming EventKitBridge; approve it. `remindctl status` through `/healthz` must then report authorized.
- **Config.**
  - Read the list ids from the log and set `read_lists` and `write_lists`.
  - Declare the places in `[places]`, with street addresses, not store names.
  - Run `eventkit-bridge install`.
- **Round trip on the write list:**
  - add an item;
  - add an item with a `place` trigger and check on the iPhone that the reminder shows the location;
  - add a timed reminder a few minutes ahead and check the iPhone notifies on time;
  - rename it, complete it and delete both;
  - a write to a list outside `write_lists` must answer `403`.
- **Groceries list type.** If the write list uses the Groceries type, check whether items added through the bridge are sorted into sections on the iPhone, and record the answer in the README.
