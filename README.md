# eventkit-bridge

`eventkit-bridge` is a small macOS daemon that exposes your Mac's calendars over an HTTP API on a private network address, such as your tailscale IP. It reads the calendars the Mac already syncs (iCloud, Google, Exchange), and it creates, updates and deletes events in exactly one calendar that you choose.

```
HTTP client (on the tailnet) --HTTP--> eventkit-bridge (LaunchAgent, EventKitBridge.app) --exec--> ekctl --EventKit--> CalendarAgent
```

The bridge does not call EventKit itself. It ships a pinned copy of [`ekctl`](https://github.com/schappim/ekctl), a native Swift command-line tool over EventKit, inside its app bundle. Each API request is turned into an `ekctl` invocation, and `ekctl`'s JSON output is turned into the bridge's own JSON. Clients never see `ekctl` and never pass it arguments.

It requires macOS 14 Sonoma or later on Apple Silicon.

## Security model

- **The network is the access control.** The bridge has no authentication. It binds only to the literal IP address in its config and refuses to listen on every interface (`0.0.0.0` or `::`). Bind it to your tailscale IP, and only devices on your tailnet can reach it.
- **Reads touch only the calendars you list.** A request for any other calendar is refused with `403`, and events from other calendars are never returned.
- **Writes touch only the one write calendar.** New events are always created in it. Before every update or delete, the bridge looks up the event and refuses the change unless the event is in the write calendar. A bug in a client cannot change an event you created yourself in another calendar.
- **The bridge builds every `ekctl` command itself.** There is no generic passthrough, so a client cannot inject options into `ekctl`.
- **Event contents are never logged.** Request logs carry the method, route, status and timing, but never titles, notes, locations, URLs or attendees.

## Install

```sh
brew install --cask pkarpovich/apps/eventkit-bridge
```

This installs `EventKitBridge.app` into `/Applications` and puts the `eventkit-bridge` command on your `PATH`.

## First run

The bridge runs as a LaunchAgent in your login session and needs a config before it first starts. You do not know your calendar ids yet, so the first run only sets the listen address; the bridge then lists your calendars in its log.

1. Create `~/.config/eventkit-bridge/config.toml` with the address to listen on, a literal IP such as your tailscale IP:

   ```toml
   listen = "100.64.0.1:8790"
   ```

2. Check the config:

   ```sh
   eventkit-bridge --check-config
   ```

   It prints what it understood and exits `0`, or prints the reason and exits `1`. It never touches your calendars.

3. Install and start the LaunchAgent:

   ```sh
   eventkit-bridge install
   ```

4. Approve the Calendars prompt for EventKitBridge.

5. Find your calendar ids in `~/Library/Logs/eventkit-bridge.log`. After it first reads your calendars, the daemon logs one line per calendar:

   ```
   INFO eventkit_bridge::server: calendar id=4F7D9489-A78F-4369-A951-213207DCFEE3 title="Calendar" source="work@example.com" readable=false writable=false
   INFO eventkit_bridge::server: calendar id=8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10 title="Agent" source="iCloud" readable=false writable=false
   ```

   Add `read_calendars` and `write_calendar` to the config, then run

   ```sh
   eventkit-bridge install
   ```

   again. The daemon restarts with the new config, and the log lines show which calendars are now readable and writable.

Upgrades need no action: the daemon restarts on the new version by itself.

### Config reference

```toml
listen = "100.64.0.1:8790"
read_calendars = ["4F7D9489-A78F-4369-A951-213207DCFEE3"]
write_calendar = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10"
# ekctl = "/path/to/ekctl"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `listen` | yes | A literal `IP:port`. Hostnames and unspecified addresses (`0.0.0.0`, `::`) are rejected. |
| `read_calendars` | no | The calendar ids reads may touch. Empty by default, which leaves the bridge unconfigured. |
| `write_calendar` | no | The only calendar writes may touch. It is always readable as well. Without it, every write is refused with `403`. |
| `ekctl` | no | The `ekctl` binary to run. Defaults to the `ekctl` next to the `eventkit-bridge` binary, which is the one inside the app bundle. Only useful for development. |

Unknown keys are rejected. The config is read once at startup; after changing it, run `eventkit-bridge install` again to restart the daemon.

### Command line

| Command | What it does |
| --- | --- |
| `eventkit-bridge` | Runs the daemon. This is what the LaunchAgent starts. |
| `eventkit-bridge install` | Validates the config, then writes and loads the LaunchAgent, replacing any existing one. |
| `eventkit-bridge uninstall` | Unloads and removes the LaunchAgent. The config is kept. |
| `eventkit-bridge --check-config` | Validates the config and exits `0` or `1`. |
| `eventkit-bridge --version` | Prints the version. |

`install` points the LaunchAgent at the real binary inside `EventKitBridge.app`, not at the Homebrew symlink, because the Calendars permission belongs to the app bundle. It warns when the binary is not inside an `.app` bundle, since the permission would then not survive an upgrade.

To remove the bridge completely, run `eventkit-bridge uninstall`, then `brew uninstall --cask --zap eventkit-bridge`, which also deletes the config, the LaunchAgent plist and the log.

## HTTP API

The examples use `http://100.64.0.1:8790`. Every response body is JSON. Every error is `{"error":"<message>"}` with the status codes below.

### Types

A calendar:

```json
{"id":"8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10","title":"Agent","source":"iCloud","color":"#34C759","writable":true}
```

An event:

```json
{
  "id": "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076",
  "title": "Standup",
  "start": "2026-10-05T11:00:00+02:00",
  "end": "2026-10-05T11:30:00+02:00",
  "all_day": false,
  "calendar": {"id": "4F7D9489-A78F-4369-A951-213207DCFEE3", "title": "Calendar"},
  "location": "Teams",
  "url": null,
  "notes": "long text",
  "availability": "busy",
  "recurring": true,
  "attendees": [{"name": "A Person", "email": "a@example.com", "role": "required", "status": "accepted"}]
}
```

- `start` and `end` keep the UTC offset EventKit reported.
- `recurring` is true for an event that belongs to a recurring series. Occurrences of a recurring event are returned one by one, and they all share the series `id`.
- A field EventKit does not provide is `null`; `attendees` is then `[]`.

A free slot:

```json
{"start":"2026-10-05T09:00:00+02:00","end":"2026-10-05T10:00:00+02:00","duration_minutes":60,"weekday":"monday"}
```

### Timestamps and ids

- Timestamps are RFC 3339 with an offset and whole seconds, such as `2026-10-05T11:00:00+02:00` or `2026-10-05T09:00:00Z`. Fractional seconds are rejected, except `.000`.
- In a query string, encode `+` as `%2B`. An unencoded `+` in the offset, which decodes to a space, is accepted too.
- Event ids contain a colon, such as `46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076`. Percent-encode an id when you put it in a path. An empty id or one with a control character is `400`.
- Unknown query parameters, unknown body fields and repeated single-valued parameters are `400`.

### `GET /v1/calendars`

Lists the readable event calendars. `writable` is true for the write calendar alone. Reminder lists and calendars outside the config are left out.

```sh
curl http://100.64.0.1:8790/v1/calendars
```

```json
{"calendars":[{"id":"4F7D9489-A78F-4369-A951-213207DCFEE3","title":"Calendar","source":"work@example.com","color":"#0088FF","writable":false},{"id":"8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10","title":"Agent","source":"iCloud","color":"#34C759","writable":true}]}
```

### `GET /v1/events`

Lists the events between `from` and `to`, in the order EventKit returns them.

| Parameter | Meaning |
| --- | --- |
| `from`, `to` | Required. `from` must be before `to`, and the range may span at most 62 days. |
| `calendar` | Optional and repeatable. The calendars to read. Without it, every readable calendar is read. |

```sh
curl 'http://100.64.0.1:8790/v1/events?from=2026-10-05T00:00:00%2B02:00&to=2026-10-12T00:00:00%2B02:00&calendar=4F7D9489-A78F-4369-A951-213207DCFEE3'
```

```json
{"events":[{"id":"46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076","title":"Standup","start":"2026-10-05T11:00:00+02:00","end":"2026-10-05T11:30:00+02:00","all_day":false,"calendar":{"id":"4F7D9489-A78F-4369-A951-213207DCFEE3","title":"Calendar"},"location":"Teams","url":null,"notes":"long text","availability":"busy","recurring":true,"attendees":[{"name":"A Person","email":"a@example.com","role":"required","status":"accepted"}]}]}
```

A `calendar` outside the readable set is `403 calendar not readable: <id>`. A request without `calendar` when nothing is readable is `403 no readable calendars configured`.

### `GET /v1/events/{id}`

Returns one event.

```sh
curl 'http://100.64.0.1:8790/v1/events/46EBD007-078C-44AD-80E9-5D55FDE5FCC8%3A1709076'
```

It is `404` when the event does not exist and `403 event is not in a readable calendar` when it lives in a calendar the bridge may not read.

### `GET /v1/free`

Finds free slots across the readable calendars, or across the calendars given with `calendar`.

| Parameter | Default | Accepted values |
| --- | --- | --- |
| `duration` | `30` | Minimum slot length in minutes, 5 to 1440. |
| `working_hours` | `09:00-17:00` | `HH:MM-HH:MM` with the start before the end, or `all`. |
| `weekdays` | `weekdays` | `weekdays`, `weekends`, `all`, or a comma-separated list of day names (`monday` or `mon`, and so on) where an item may be a forward range such as `mon-fri`. Case-insensitive. |
| `buffer` | `0` | Minutes kept free around busy events, 0 to 240. |
| `limit` | `20` | Maximum number of slots, 1 to 100. |
| `from`, `to` | now to 7 days ahead | RFC 3339. When both are given, `from` must be before `to` and the range may span at most 62 days. |
| `calendar` | every readable calendar | Repeatable, same rules as `/v1/events`. |

```sh
curl 'http://100.64.0.1:8790/v1/free?duration=60&weekdays=mon-wed,fri&buffer=15'
```

```json
{"slots":[{"start":"2026-10-05T09:00:00+02:00","end":"2026-10-05T10:00:00+02:00","duration_minutes":60,"weekday":"monday"}],"searched_from":"2026-10-04T21:40:21+02:00","searched_to":"2026-10-11T21:40:21+02:00"}
```

### `POST /v1/events`

Creates an event in the write calendar and answers `201` with the complete event.

```sh
curl -X POST http://100.64.0.1:8790/v1/events \
  -H 'Content-Type: application/json' \
  -d '{"title":"Lunch","start":"2026-10-06T12:30:00+02:00","end":"2026-10-06T13:30:00+02:00","location":"Cafe","notes":"Booked","url":"https://example.com/booking"}'
```

`title`, `start` and `end` are required; `location`, `notes` and `url` are optional. Only timed events can be created: no all-day events, recurrence, attendees or alarms. Without a `write_calendar` in the config, the answer is `403 no write calendar configured`.

### `PATCH /v1/events/{id}`

Changes any non-empty subset of `title`, `start`, `end`, `location`, `notes` and `url`, and answers `200` with the complete event. A field that is absent or `null` stays unchanged.

```sh
curl -X PATCH 'http://100.64.0.1:8790/v1/events/NEW123%3AEVENT456' \
  -H 'Content-Type: application/json' \
  -d '{"start":"2026-10-06T13:00:00+02:00"}'
```

The bridge first looks the event up. It answers `403 event is not in the write calendar` unless the event is in the write calendar, and `404` when it does not exist. The new or existing start must still be before the new or existing end. A body that changes nothing is `400`.

### `DELETE /v1/events/{id}`

Deletes an event in the write calendar and answers `204` with no body. The same lookup and `403`/`404` rules as `PATCH` apply.

```sh
curl -X DELETE 'http://100.64.0.1:8790/v1/events/NEW123%3AEVENT456'
```

### Write validation

A write is `400` when:

- `title` is blank or longer than 500 characters;
- `start` or `end` is not an RFC 3339 timestamp, or `end` is not after `start`;
- `location` is longer than 500 characters or `notes` longer than 10 000;
- `url` is not an absolute `http` or `https` URL, or is longer than 2 000 characters;
- any string contains a control character other than newline and tab;
- the body is not valid JSON or has an unknown field.

A body larger than 64 KiB is `413`.

### Status codes

| Status | Meaning |
| --- | --- |
| `400` | The request is invalid; the message names the parameter or field. |
| `403` | The security policy refused the request. |
| `404` | The event or the route does not exist. |
| `405` | The route does not accept the method. |
| `413` | The request body is larger than 64 KiB. |
| `502` | `ekctl` failed: it could not start, exited with an error, reported an error, wrote more than 8 MiB, or wrote output the bridge does not understand. |
| `504` | `ekctl` did not finish within 20 seconds. |

Requests are handled one `ekctl` call at a time. A week across every calendar takes about 0.2 seconds.

## Health check

```sh
curl http://100.64.0.1:8790/healthz
```

When the bridge can read every configured calendar, it answers `200`:

```json
{"status":"ok","version":"0.1.0","calendars":2}
```

`calendars` is the number of readable calendars that exist. Otherwise it answers `503`:

```json
{"status":"degraded","reason":"ekctl failed"}
```

| `reason` | Meaning |
| --- | --- |
| `unconfigured` | `read_calendars` is empty. |
| `timeout` | `ekctl` did not answer in time. |
| `ekctl failed` | `ekctl` could not list calendars. This is how a missing or revoked Calendars permission shows up. |
| `calendar missing` | A configured calendar id does not exist, or is not an event calendar. |

The check runs `ekctl` at most once every 10 seconds and reuses the result in between, so it is safe to poll.

## Upgrades

There is nothing to do. `brew upgrade --cask eventkit-bridge` replaces the app; the running daemon notices within a couple of seconds that its binary changed, finishes the requests in flight and exits, and launchd starts the new version. The Calendars permission belongs to the app bundle and carries over.

## Logs

The daemon logs to `~/Library/Logs/eventkit-bridge.log`. Each request produces one line with the method, the route template, the status, the duration and, when `ekctl` ran, each subcommand with its exit code:

```
INFO eventkit_bridge::server: request method=PATCH route=/v1/events/{id} status=403 duration_ms=212 ekctl="show event=0"
```

Policy refusals log their reason. Event titles, notes, locations, URLs and attendees are never logged; the only calendar details in the log are the startup listing of ids, titles and accounts.

The log is not rotated. To truncate it:

```sh
: > ~/Library/Logs/eventkit-bridge.log
```

## Troubleshooting

- **`/healthz` says `ekctl failed`.** The bridge most likely has no Calendars permission. Open System Settings, Privacy & Security, Calendars, and check that EventKitBridge has full access. If it is missing or the prompt never appeared, reset the permission and reinstall the agent to get a fresh prompt:

  ```sh
  tccutil reset Calendar dev.pkarpovich.eventkit-bridge
  eventkit-bridge install
  ```

- **`/healthz` says `calendar missing`.** A calendar in the config was deleted or its id changed. Look up the current ids in the startup listing in the log and update the config.
- **The bridge is unreachable after a reboot.** It is a LaunchAgent, so it runs only in your login session. With FileVault on, it starts only after you log in following a reboot. If it starts before tailscale is up, the bind fails, and launchd keeps restarting it until the address exists.
- **`install` warns that the program is not inside an `.app` bundle.** You ran a binary from somewhere other than the installed app. The Calendars permission is tied to `EventKitBridge.app` and would not survive an upgrade; run `install` from the Homebrew-installed `eventkit-bridge`.

## Releasing

1. Bump `version` in `Cargo.toml` and commit.
2. Tag the commit with the same version and push the tag:

   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```

The release workflow checks that the tag matches `Cargo.toml`, runs the checks, builds the Apple Silicon binary, fetches and verifies the pinned `ekctl`, signs the app with the Developer ID certificate and the hardened runtime, notarizes and staples it, publishes a GitHub release with the zip and `checksums.txt`, and writes the new cask into `pkarpovich/homebrew-apps`.

It needs seven repository secrets:

| Secret | Where it comes from |
| --- | --- |
| `MACOS_CERT_P12_BASE64` | The `Developer ID Application` certificate with its private key, exported from Keychain Access as a `.p12` and base64-encoded. |
| `MACOS_CERT_PASSWORD` | The password chosen when exporting the `.p12`. |
| `MACOS_TEAM_ID` | The Apple Developer team id, the ten characters in parentheses in the certificate name. |
| `ASC_KEY_ID` | The key id of an App Store Connect API key (Users and Access, Integrations), used by `notarytool`. |
| `ASC_ISSUER_ID` | The issuer id shown on the same App Store Connect page. |
| `ASC_KEY_CONTENT` | The contents of the downloaded `AuthKey_<key id>.p8` file. |
| `HOMEBREW_TAP_TOKEN` | A GitHub token with write access to `pkarpovich/homebrew-apps`. |

The maintainer keeps these in the 1Password item `nhop release signing`, which feeds other repositories too.

For a local signed build, run `scripts/build-signed.sh <team-id>`. It builds the release binary, fetches `ekctl`, and signs `dist/EventKitBridge.app` with the Developer ID identity from your login keychain.

## Development

The crate builds and its tests pass on macOS and Linux; no test runs `ekctl` or `launchctl` for real.

```sh
mise run check
```

runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. CI also runs `shellcheck scripts/*.sh`.

## Credits

The bridge runs [`ekctl`](https://github.com/schappim/ekctl) by schappim, released under the MIT License. The app bundle includes its license as `Contents/Resources/ekctl-LICENSE.txt`.

## License

MIT. See [LICENSE](LICENSE).
