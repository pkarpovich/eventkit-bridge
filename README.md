# eventkit-bridge

`eventkit-bridge` is a small macOS daemon that exposes your Mac's calendars and reminders over an HTTP API on a private network address, such as your tailscale IP. It reads the calendars the Mac already syncs (iCloud, Google, Exchange), and it creates, updates and deletes events in the calendars you choose. It reads the reminder lists you allow, and it adds, changes, completes and deletes reminders in the lists you choose, with a due date, a repeat rule, a priority and an optional location trigger.

```
HTTP client (on the tailnet) --HTTP--> eventkit-bridge (LaunchAgent, EventKitBridge.app) --exec--> ekctl     --EventKit--> calendars
                                                                                          --exec--> remindctl --EventKit--> reminders
```

The bridge does not call EventKit itself. It ships pinned copies of two native Swift command-line tools over EventKit inside its app bundle: [`ekctl`](https://github.com/schappim/ekctl) v1.8.0 for calendars and [`remindctl`](https://github.com/openclaw/remindctl) v0.3.8 for reminders. Each API request is turned into an `ekctl` or `remindctl` invocation, and the tool's JSON output is turned into the bridge's own JSON. Clients never see either tool and never pass it arguments.

It requires macOS 14 Sonoma or later on Apple Silicon.

## Security model

- **The network is the access control.** The bridge has no authentication. It binds only to the literal IP address in its config and refuses to listen on every interface (`0.0.0.0`, `::` or `::ffff:0.0.0.0`). Bind it to your tailscale IP, and only devices on your tailnet can reach it.
- **Requests must name the bridge.** The `Host` header must be the listen IP or a name listed in `hosts`; anything else is refused with `421`. This stops a web page open in a browser on the tailnet from reaching the bridge through DNS rebinding.
- **Reads touch only the calendars you list.** A request for any other calendar is refused with `403`, and events from other calendars are never returned.
- **Writes touch only the write calendars.** A new event must name one of them. Before every update or delete, the bridge looks up the event and refuses the change unless the event is in a write calendar. A bug in a client cannot change an event in any other calendar.
- **Recurring events are not changed.** `ekctl` looks an event up by id, and for a recurring event that is the first occurrence of the series, so an update or delete would silently hit the wrong occurrence. The bridge refuses both with `409` until `ekctl` can address a single occurrence.
- **Reminders follow the same rules.** Reads touch only the reminder lists you list, and writes only the write lists. Before every change or delete, the bridge looks up the reminder and refuses unless it is in a write list.
- **Location triggers name a place from the config.** A client picks a place such as `shop` by name; street addresses and coordinates never cross the API in either direction.
- **The bridge builds every `ekctl` and `remindctl` command itself.** There is no generic passthrough, so a client cannot inject options into either tool. Reminder and list ids must be full UUIDs, because `remindctl` reads a short number as a row index from its last listing.
- **Contents are never logged.** Request logs carry the method, route, status and timing, but never event titles, notes, locations, URLs or attendees, reminder titles or notes, or place addresses and coordinates.

## Install

```sh
brew install --cask pkarpovich/apps/eventkit-bridge
```

This installs `EventKitBridge.app` into `/Applications` and puts the `eventkit-bridge` command on your `PATH`.

## First run

The bridge runs as a LaunchAgent in your login session and needs a config before it first starts. You do not know your calendar and list ids yet, so the first run only sets the listen address; the bridge then lists your calendars and reminder lists in its log.

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

4. Approve the Calendars prompt for EventKitBridge, then the Reminders prompt. Reminders is a separate permission, so macOS asks twice.

5. Find your calendar ids in `~/Library/Logs/eventkit-bridge.log`. After it first reads your calendars, the daemon logs one line per calendar:

   ```
   2026-10-05T09:00:01.204518Z  INFO eventkit_bridge::server: calendar id=4F7D9489-A78F-4369-A951-213207DCFEE3 title="Calendar" source="work@example.com" readable=false writable=false
   2026-10-05T09:00:01.204533Z  INFO eventkit_bridge::server: calendar id=8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10 title="Agent" source="iCloud" readable=false writable=false
   ```

   Until the permission is granted, the log shows `cannot list calendars yet, retrying` instead. The daemon retries every 30 seconds, so the listing appears within half a minute of approving the prompt.

   After the calendars, the daemon logs one line per configured place, by name and radius, then one line per reminder list, in the same way:

   ```
   2026-10-05T09:00:02.317912Z  INFO eventkit_bridge::server: place name=shop radius=150
   2026-10-05T09:00:02.318040Z  INFO eventkit_bridge::server: reminder list id=2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37 title="Shopping" readable=false writable=false
   ```

   Until the Reminders prompt is approved, it shows `cannot list reminder lists yet, retrying`.

   Add `read_calendars` and `write_calendars` to the config, and `read_lists` and `write_lists` if you want reminders, then run

   ```sh
   eventkit-bridge install
   ```

   again. The daemon restarts with the new config, and the log lines show which calendars are now readable and writable.

Upgrades need no action: the daemon restarts on the new version by itself. After upgrading from a version without reminders, the first reminders call shows the Reminders prompt once; approve it.

### Config reference

```toml
listen = "100.64.0.1:8790"
# hosts = ["mac.tail1234.ts.net"]
read_calendars = ["4F7D9489-A78F-4369-A951-213207DCFEE3"]
write_calendars = ["8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10"]
read_lists = ["5E2B8D17-3C4A-4F90-A6B1-7D8E9F0A1B2C"]
write_lists = ["2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37"]
# ekctl = "/path/to/ekctl"
# remindctl = "/path/to/remindctl"

[places]
shop = { address = "1 Example Street, Exampletown", radius = 150 }
home = { address = "2 Example Road, Exampletown" }
```

| Key | Required | Meaning |
| --- | --- | --- |
| `listen` | yes | A literal `IP:port`. Hostnames and unspecified addresses (`0.0.0.0`, `::`, `::ffff:0.0.0.0`) are rejected. |
| `hosts` | no | Extra names clients may use in the `Host` header, such as the Mac's MagicDNS name. The listen IP is always accepted. Names are compared case-insensitively; the port is not checked. |
| `read_calendars` | no | The calendar ids reads may touch. Empty by default, which leaves the bridge unconfigured. |
| `write_calendars` | no | The calendar ids writes may touch. They are always readable as well. Empty by default, which refuses every write with `403`. |
| `ekctl` | no | The `ekctl` binary to run. Defaults to the `ekctl` next to the `eventkit-bridge` binary, which is the one inside the app bundle. Only useful for development. |
| `read_lists` | no | The reminder list ids reads may touch, as full UUIDs. Empty by default. With no lists at all, reminders are off and `remindctl` is only run for the startup listing. |
| `write_lists` | no | The reminder list ids writes may touch. They are always readable as well. Empty by default, which refuses every reminder write with `403`. |
| `places` | no | Named places for location triggers. A name is 1 to 40 characters of lowercase letters, digits and `-`, starting with a letter or digit. `address` is a street address and must not be blank or contain a control character; `radius` is in meters, 50 to 2000, default 100. |
| `remindctl` | no | The `remindctl` binary to run. Defaults to the `remindctl` next to the `eventkit-bridge` binary. Only useful for development. |

Unknown keys are rejected. The config is read once at startup; after changing it, run `eventkit-bridge install` again to restart the daemon.

`--check-config` prints the readable and writable calendars and lists and the place names with their radii. Place addresses are never printed or logged.

#### Why places are named

A location trigger names a place from the config rather than taking an address from the client. CoreLocation, which `remindctl` uses to geocode, resolves street addresses but not store names: `Some Store` fails, its street address works. Named places keep geocoding predictable, and they keep home and shop addresses out of client logs and agent context. Give each place a street address, not a business name.

### Command line

| Command | What it does |
| --- | --- |
| `eventkit-bridge` | Runs the daemon. This is what the LaunchAgent starts. |
| `eventkit-bridge install` | Validates the config, then writes and loads the LaunchAgent, replacing any existing one. |
| `eventkit-bridge uninstall` | Unloads and removes the LaunchAgent. The config is kept. |
| `eventkit-bridge --check-config` | Validates the config and exits `0` or `1`. |
| `eventkit-bridge --version` | Prints the version. |

`install` points the LaunchAgent at the real binary inside `EventKitBridge.app`, not at the Homebrew symlink, because the Calendars and Reminders permissions belong to the app bundle. It warns when the binary is not inside an `.app` bundle, since the permission would then not survive an upgrade.

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

Lists the readable event calendars. `writable` is true for the write calendars. Reminder lists and calendars outside the config are left out.

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
| `calendar` | Optional and repeatable. The calendars to read, one id per parameter (`calendar=A&calendar=B`); a comma-separated list or an empty value is `400`. Without it, every readable calendar is read. |

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
| `from`, `to` | now to 7 days ahead | RFC 3339. Give both or neither; `from` must be before `to` and the range may span at most 62 days. |
| `calendar` | every readable calendar | Repeatable, same rules as `/v1/events`. |

```sh
curl 'http://100.64.0.1:8790/v1/free?duration=60&weekdays=mon-wed,fri&buffer=15'
```

```json
{"slots":[{"start":"2026-10-05T09:00:00+02:00","end":"2026-10-05T10:00:00+02:00","duration_minutes":60,"weekday":"monday"}],"searched_from":"2026-10-04T21:40:21+02:00","searched_to":"2026-10-11T21:40:21+02:00"}
```

### `POST /v1/events`

Creates an event in the write calendar named by `calendar` and answers `201` with the complete event.

```sh
curl -X POST http://100.64.0.1:8790/v1/events \
  -H 'Content-Type: application/json' \
  -d '{"calendar":"8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10","title":"Lunch","start":"2026-10-06T12:30:00+02:00","end":"2026-10-06T13:30:00+02:00","location":"Cafe","notes":"Booked","url":"https://example.com/booking"}'
```

`calendar`, `title`, `start` and `end` are required; `location`, `notes` and `url` are optional. Only timed events can be created: no all-day events, recurrence, attendees or alarms. A `calendar` outside `write_calendars` is `403 calendar not writable: <id>`, and with no `write_calendars` at all the answer is `403 no write calendars configured`.

### `PATCH /v1/events/{id}`

Changes any non-empty subset of `title`, `start`, `end`, `location`, `notes` and `url`, and answers `200` with the complete event. A field that is absent or `null` stays unchanged.

```sh
curl -X PATCH 'http://100.64.0.1:8790/v1/events/NEW123%3AEVENT456' \
  -H 'Content-Type: application/json' \
  -d '{"start":"2026-10-06T13:00:00+02:00"}'
```

The bridge first looks the event up. It answers `403 event is not in a writable calendar` unless the event is in a write calendar, `409` when the event is recurring, and `404` when it does not exist. The new or existing start must still be before the new or existing end. A body that changes nothing is `400`.

### `DELETE /v1/events/{id}`

Deletes an event in a write calendar and answers `204` with no body. The same lookup and `403`/`404`/`409` rules as `PATCH` apply.

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

A body larger than 64 KiB is `413`. `POST` and `PATCH` must send `Content-Type: application/json`; any other content type, or none, is `415`. This stops a web page open in a browser on the tailnet from creating events with a cross-site form or `fetch` request.

### Status codes

| Status | Meaning |
| --- | --- |
| `400` | The request is invalid; the message names the parameter or field. |
| `403` | The security policy refused the request. |
| `404` | The event, the reminder or the route does not exist. |
| `409` | `PATCH` or `DELETE` on a recurring event. |
| `405` | The route does not accept the method. |
| `413` | The request body is larger than 64 KiB. |
| `415` | A `POST` or `PATCH` without `Content-Type: application/json`. |
| `421` | The `Host` header is missing or names neither the listen IP nor an entry in `hosts`. |
| `502` | `ekctl` or `remindctl` failed: it could not start, exited with an error, reported an error, wrote more than 8 MiB, or wrote output the bridge does not understand. A configured reminder list that no longer exists is also `502`. |
| `504` | `ekctl` or `remindctl` did not finish within 20 seconds. |

Requests are handled one `ekctl` or `remindctl` call at a time; both tools share one lock because they write to the same EventKit store. A week across every calendar takes about 0.2 seconds.

## Reminders API

The reminder routes follow the same rules as the calendar routes: the `Host` check, `Content-Type: application/json` on `POST` and `PATCH`, the 64 KiB body limit, `{"error": ...}` errors, and `400` for unknown query parameters, unknown body fields and control characters other than newline and tab.

### Types

A list:

```json
{"id":"2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37","title":"Shopping","open":12,"writable":true}
```

`open` is the number of incomplete reminders.

A reminder:

```json
{
  "id": "3D4E5F6A-7B8C-4D9E-BF0A-2B3C4D5E6F7A",
  "title": "Milk",
  "notes": null,
  "completed": false,
  "completed_at": null,
  "due": "2026-10-06T09:00:00+02:00",
  "all_day": false,
  "repeat": "weekly",
  "priority": "none",
  "list": {"id": "2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37", "title": "Shopping"},
  "location": {"place": "shop", "proximity": "arriving"}
}
```

- `due` is `null` without a due date. A timed reminder's `due` is RFC 3339 in the Mac's time zone. An all-day reminder's `due` is `YYYY-MM-DD` with `all_day: true`. `completed_at` is RFC 3339 in the Mac's time zone, or `null`.
- `repeat` is `null` or one of `daily`, `weekly`, `biweekly`, `monthly` and `yearly`. A rule set elsewhere with another frequency or interval is reported as `custom`.
- `priority` is `none`, `low`, `medium` or `high`.
- `location` is `null` without a location trigger. `place` is the config name whose address matches the trigger; a trigger that matches no configured place, such as one set on a phone, has `"place": null` and only its `proximity`. Addresses and coordinates are never returned.

Reminder and list ids are full UUIDs, such as `3D4E5F6A-7B8C-4D9E-BF0A-2B3C4D5E6F7A`. Anything else is `400`. Letter case does not matter; the bridge reports ids in uppercase, as `remindctl` does.

### `GET /v1/lists`

Lists the readable reminder lists. `writable` is true for the write lists.

```sh
curl http://100.64.0.1:8790/v1/lists
```

```json
{"lists":[{"id":"2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37","title":"Shopping","open":12,"writable":true}]}
```

With no lists configured it answers `{"lists":[]}` without running `remindctl`.

### `GET /v1/places`

Lists the configured places by name and radius in meters, sorted by name.

```sh
curl http://100.64.0.1:8790/v1/places
```

```json
{"places":[{"name":"home","radius":100},{"name":"shop","radius":150}]}
```

### `GET /v1/reminders`

Lists reminders in `remindctl`'s order.

| Parameter | Meaning |
| --- | --- |
| `status` | `open` (default), `completed` or `all`. |
| `list` | Optional and repeatable, one list id per parameter. Without it, every readable list is read. |

```sh
curl 'http://100.64.0.1:8790/v1/reminders?status=all&list=2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37'
```

```json
{"reminders":[{"id":"3D4E5F6A-7B8C-4D9E-BF0A-2B3C4D5E6F7A","title":"Milk","notes":null,"completed":false,"completed_at":null,"due":null,"all_day":false,"repeat":null,"priority":"none","list":{"id":"2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37","title":"Shopping"},"location":null}]}
```

A `list` outside the readable set is `403 list not readable: <id>`. A request without `list` when nothing is readable is `403 no readable lists configured`.

### `GET /v1/reminders/{id}`

Returns one reminder. It is `404` when the reminder does not exist and `403 reminder is not in a readable list` when it lives in a list the bridge may not read. With no lists configured it answers `403 no readable lists configured` without running `remindctl`.

### `POST /v1/reminders`

Creates a reminder in the write list named by `list` and answers `201` with the reminder.

```sh
curl -X POST http://100.64.0.1:8790/v1/reminders \
  -H 'Content-Type: application/json' \
  -d '{"list":"2A7C1E90-5B3D-4F68-8E21-9D4C6B0A1F37","title":"Call the dentist","due":"2026-10-06T09:00:00+02:00","priority":"high"}'
```

| Field | Rules |
| --- | --- |
| `list` | Required. A write list; any other list is `403 list not writable: <id>`, and with no `write_lists` at all the answer is `403 no write lists configured`. |
| `title` | Required. Not blank, at most 500 characters. |
| `notes` | At most 10 000 characters. |
| `due` | An RFC 3339 date-time with an offset and whole seconds, which makes a timed reminder that notifies at that time, or `YYYY-MM-DD`, which makes an all-day reminder without a notification. |
| `repeat` | `daily`, `weekly`, `biweekly`, `monthly` or `yearly`. Needs `due`. |
| `priority` | `none`, `low`, `medium` or `high`. |
| `place` | The name of a configured place, which adds a location trigger. Geocoding the place's address needs the Mac to be online. |
| `proximity` | `arriving` (default) or `leaving`. Only allowed with `place`. |

A location trigger can only be set when the reminder is created.

### `PATCH /v1/reminders/{id}`

Changes any non-empty subset of `title`, `notes`, `due`, `repeat`, `priority` and `completed`, and answers `200` with the reminder. The fields follow the `POST` rules. Changing `due` moves the notification with it: a new date-time notifies at that time, and a new `YYYY-MM-DD` or `due: null` removes the timed notification. `due: null` removes the due date and `repeat: null` removes the repeat rule; for the other fields, `null` or an absent field leaves the value unchanged. `completed: true` completes the reminder, `false` reopens it.

```sh
curl -X PATCH http://100.64.0.1:8790/v1/reminders/3D4E5F6A-7B8C-4D9E-BF0A-2B3C4D5E6F7A \
  -H 'Content-Type: application/json' \
  -d '{"completed":true}'
```

The bridge first looks the reminder up. It answers `403 reminder is not in a writable list` unless the reminder is in a write list, and `404` when it does not exist. A change that would leave a repeat rule without a due date is `400`. A body that changes nothing is `400`.

### `DELETE /v1/reminders/{id}`

Deletes a reminder in a write list and answers `204` with no body. The same lookup and `403`/`404` rules as `PATCH` apply.

### Not available

Reminder sections, tags, subtasks, smart lists, the Groceries list type and the Urgent toggle have no public EventKit API, so neither `remindctl` nor the bridge can read or set them. Alarms separate from the due time, URLs, custom repeat rules, moving a reminder between lists, and creating or renaming lists are not supported either.

## Health check

```sh
curl http://100.64.0.1:8790/healthz
```

When the bridge can read every configured calendar, it answers `200`:

```json
{"status":"ok","version":"0.3.0","calendars":2,"lists":1}
```

`calendars` is the number of readable calendars that exist. When reminder lists are configured, the check also runs `remindctl`, and `lists` is the number of readable lists that exist; without lists, `lists` is left out and `remindctl` is not run. Unlike a missing calendar, a configured list that no longer exists does not make the check degraded; it only lowers `lists`, so compare `lists` with the number of lists in your config. Otherwise it answers `503`:

```json
{"status":"degraded","reason":"ekctl failed"}
```

| `reason` | Meaning |
| --- | --- |
| `unconfigured` | `read_calendars` is empty. |
| `timeout` | `ekctl` did not answer in time. |
| `ekctl failed` | `ekctl` could not list calendars. This is how a missing or revoked Calendars permission shows up. |
| `calendar missing` | A configured calendar id does not exist, or is not an event calendar. |
| `reminders access missing` | Reminder lists are configured, but `remindctl status` reports no Reminders permission. |
| `remindctl failed` | Reminder lists are configured and `remindctl` could not report its status or list the reminder lists. |

`timeout` covers `remindctl` as well. The check runs `ekctl` (and `remindctl`) at most once every 10 seconds and reuses the result in between, so it is safe to poll.

## Upgrades

There is nothing to do. `brew upgrade --cask eventkit-bridge` replaces the app; the running daemon notices within a couple of seconds that its binary changed, finishes the requests in flight and exits, and launchd starts the new version. The Calendars and Reminders permissions belong to the app bundle and carry over.

## Logs

The daemon logs to `~/Library/Logs/eventkit-bridge.log`. Each request produces one line with the method, the route template, the status, the duration and, when `ekctl` or `remindctl` ran, each subcommand with its exit code:

```
2026-10-05T09:12:40.881207Z  INFO eventkit_bridge::server: request method=PATCH route=/v1/events/{id} status=403 duration_ms=212 ekctl="show event=0"
2026-10-05T09:13:02.104377Z  INFO eventkit_bridge::server: request method=PATCH route=/v1/reminders/{id} status=200 duration_ms=164 remindctl="info=0, edit=0"
```

Policy refusals log their reason. Event titles, notes, locations, URLs and attendees, reminder titles and notes, and place addresses and coordinates are never logged. The only calendar and reminder details in the log are the startup listing of calendar ids, titles and accounts, reminder list ids and titles, and place names with their radii.

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

- **`/healthz` says `reminders access missing`.** The bridge has no Reminders permission. Check System Settings, Privacy & Security, Reminders, or reset it to get a fresh prompt:

  ```sh
  tccutil reset Reminders dev.pkarpovich.eventkit-bridge
  eventkit-bridge install
  ```

- **A reminder with a `place` fails with `502`.** `remindctl` could not geocode the place's address. Check that the Mac is online and that the address is a street address, not a business name.
- **A reminder request fails with `502 remindctl: List not found`.** A list in `read_lists` or `write_lists` was deleted or its id changed. Look up the current ids in the startup listing in the log and update the config.
- **`/healthz` says `calendar missing`.** A calendar in the config was deleted or its id changed. Look up the current ids in the startup listing in the log and update the config.
- **The bridge is unreachable after a reboot.** It is a LaunchAgent, so it runs only in your login session. With FileVault on, it starts only after you log in following a reboot. If it starts before tailscale is up, the bind fails, and launchd keeps restarting it until the address exists.
- **`install` warns that the program is not inside an `.app` bundle.** You ran a binary from somewhere other than the installed app. The Calendars and Reminders permissions are tied to `EventKitBridge.app` and would not survive an upgrade; run `install` from the Homebrew-installed `eventkit-bridge`.

## Releasing

1. Bump `version` in `Cargo.toml` and commit.
2. Tag the commit with the same version and push the tag:

   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```

The release workflow checks that the tag matches `Cargo.toml`, runs the checks, builds the Apple Silicon binary, fetches and verifies the pinned `ekctl` and `remindctl`, signs both tools and the app with the Developer ID certificate and the hardened runtime, notarizes and staples it, publishes a GitHub release with the zip and `checksums.txt`, and writes the new cask into `pkarpovich/homebrew-apps`.

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

The maintainer keeps these in a password manager, outside the repository.

For a local signed build, run `scripts/build-signed.sh <team-id>`. It builds the release binary, fetches `ekctl` and `remindctl`, and signs `dist/EventKitBridge.app` with the Developer ID identity from your login keychain.

## Development

The crate builds and its tests pass on macOS and Linux; no test runs `ekctl`, `remindctl` or `launchctl` for real.

```sh
mise run check
```

runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. CI also runs `shellcheck scripts/*.sh`.

Do not run the daemon, `ekctl` or `remindctl` from a terminal against your real calendars or reminders: TCC would grant access to the terminal app, not to EventKitBridge. Test on the Mac with the bundled app started by the LaunchAgent.

## Credits

The bridge runs [`ekctl`](https://github.com/schappim/ekctl) by schappim and [`remindctl`](https://github.com/openclaw/remindctl) by OpenClaw, both released under the MIT License. The app bundle includes their licenses as `Contents/Resources/ekctl-LICENSE.txt` and `Contents/Resources/remindctl-LICENSE.txt`.

## License

MIT. See [LICENSE](LICENSE).
