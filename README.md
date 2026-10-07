# eventkit-bridge

`eventkit-bridge` is a small macOS daemon that exposes your Mac's calendars and reminders over an HTTP API on a private network address, such as your tailscale IP. It reads the calendars the Mac already syncs (iCloud, Google, Exchange), and it creates, updates and deletes events in the calendars you choose. It reads the reminder lists you allow, and it adds, changes, completes and deletes reminders in the lists you choose, with a due date, a repeat rule, a priority and an optional location trigger. Optionally, it also reads the mail Apple Mail has already downloaded, for every account configured in Mail, and asks Mail to move a message to junk or back to the inbox; it never writes Mail's files.

```
HTTP client (on the tailnet) --HTTP--> eventkit-bridge (LaunchAgent, EventKitBridge.app) --exec--> ekctl     --EventKit--> calendars
                                                                                          --exec--> remindctl --EventKit--> reminders
                                                                                          --read-only-------------------> ~/Library/Mail
                                                                                          --exec--> osascript --Apple Events--> Mail.app (junk / not junk)
```

The bridge does not call EventKit itself. It ships pinned copies of two native Swift command-line tools over EventKit inside its app bundle: [`ekctl`](https://github.com/schappim/ekctl) v1.8.0 for calendars and [`remindctl`](https://github.com/openclaw/remindctl) v0.3.8 for reminders. Each API request is turned into an `ekctl` or `remindctl` invocation, and the tool's JSON output is turned into the bridge's own JSON. Clients never see either tool and never pass it arguments.

Mail is read straight from the files Mail keeps on disk: the Envelope Index, a SQLite database with one row per message, and the `.emlx` files that hold each message. The one mail write, marking a message as junk or not junk, goes through Mail.app over Apple Events, so Mail moves the message and syncs the move to the server as if you had clicked Junk in Mail. The bridge never talks to a mail server itself and needs no account credentials.

It requires macOS 14 Sonoma or later on Apple Silicon.

## Security model

- **The network is the first access control.** The bridge binds only to the literal IP address in its config and refuses to listen on every interface (`0.0.0.0`, `::` or `::ffff:0.0.0.0`). Bind it to your tailscale IP, and only devices on your tailnet can reach it.
- **Requests must name the bridge.** The `Host` header must be the listen IP or a name listed in `hosts`; anything else is refused with `421`. This stops a web page open in a browser on the tailnet from reaching the bridge through DNS rebinding.
- **An optional bearer token is the second gate.** With an `[auth]` table, every request except `/healthz` must carry an OAuth 2.0 access token from your OpenID provider, and each route needs one scope (see Authentication). The bridge validates the token itself against the provider's published keys; it never sees a client secret. The token adds to the network bind and the `Host` check, which still run first; it does not replace them. Without `[auth]`, the network is the only access control.
- **Reads touch only the calendars you list.** A request for any other calendar is refused with `403`, and events from other calendars are never returned.
- **Writes touch only the write calendars.** A new event must name one of them. Before every update or delete, the bridge looks up the event and refuses the change unless the event is in a write calendar. A bug in a client cannot change an event in any other calendar.
- **Recurring events are not changed.** `ekctl` looks an event up by id, and for a recurring event that is the first occurrence of the series, so an update or delete would silently hit the wrong occurrence. The bridge refuses both with `409` until `ekctl` can address a single occurrence.
- **Reminders follow the same rules.** Reads touch only the reminder lists you list, and writes only the write lists. Before every change or delete, the bridge looks up the reminder and refuses unless it is in a write list.
- **Location triggers name a place from the config.** A client picks a place such as `shop` by name; street addresses and coordinates never cross the API in either direction.
- **The bridge builds every `ekctl` and `remindctl` command itself.** There is no generic passthrough, so a client cannot inject options into either tool. Reminder and list ids must be full UUIDs, because `remindctl` reads a short number as a row index from its last listing.
- **Mail is off unless you turn it on, and then read from disk read-only.** Without a `[mail]` table no mail code runs. With it, the Envelope Index is opened read-only, no message data is ever written by the bridge (the only files the bridge can create under `~/Library/Mail` are SQLite's own `-shm` and `-wal` files next to the Envelope Index, when Mail is not running), and only the accounts you name in `[mail.accounts]` are visible.
- **The single mail write is junk or not junk, done by Mail.app.** `PATCH /v1/mail/messages/{id}` asks Mail over Apple Events to move one visible message to its account's junk mailbox or back to its inbox. The bridge runs `/usr/bin/osascript` directly with a fixed script and passes the account, mailboxes and message id as separate arguments, never as script text. Sending, replying, deleting, flagging, marking read and moving to any other mailbox are not possible. macOS asks once before EventKitBridge may control Mail.
- **Reading mail needs Full Disk Access, for the whole bridge.** macOS has no narrower permission for `~/Library/Mail`. Granting it to EventKitBridge lets the bridge process read every file your user can, not just mail. The bridge itself only opens the Envelope Index, the `.emlx` files under the mail folder and `~/Library/Accounts/Accounts4.sqlite`, and refuses any message path that leads outside the mail folder. Leave mail off if you do not want to grant it.
- **Contents are never logged.** Request logs carry the method, route, status and timing, the client id of a bearer token, plus a row count for mail reads and the account name and junk value for junk requests, but never event titles, notes, locations, URLs or attendees, reminder titles or notes, place addresses and coordinates, or mail addresses, names, subjects, summaries, bodies, attachment names, Message-IDs and mailbox paths. Tokens and their claims, other than the client id, are never logged either.

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

Upgrades usually need no action: the daemon restarts on the new version by itself (see Upgrades for the exceptions). After upgrading from a version without reminders, the first reminders call shows the Reminders prompt once; approve it.

### Turning on mail

Mail is optional. Skip this section to keep it off.

1. Open System Settings, Privacy & Security, Full Disk Access, click `+`, and add `EventKitBridge` from `/Applications`. There is no prompt for this permission; it has to be added by hand. It applies to the whole bridge process, not just mail (see Security model).

2. Add an empty `[mail]` table to the config and reinstall:

   ```toml
   [mail]
   ```

   ```sh
   eventkit-bridge install
   ```

3. Find your mail accounts in `~/Library/Logs/eventkit-bridge.log`. The daemon logs one line per account found in Mail's store:

   ```
   2026-10-05T09:00:01.512930Z  INFO eventkit_bridge::server: mail account id=1B9F0E52-6C3A-4D27-8E45-7A0B2C9D1E83 kind=exchange account_type="com.apple.account.Exchange" description="Work" mailboxes=24 messages=18342 newest="2026-10-05T10:58:12+02:00"
   2026-10-05T09:00:01.512977Z  INFO eventkit_bridge::server: mail account id=6E2D8A17-4B90-4C3F-A1D5-9F8E7C6B5A42 kind=imap account_type="com.apple.account.IMAP" description="me@gmail.example" mailboxes=9 messages=40211 newest="2026-10-05T10:57:40+02:00"
   ```

   `kind` is `exchange`, `imap` or `local`. `account_type` and `description` come from `~/Library/Accounts/Accounts4.sqlite` and are left out when it cannot be read; the description of an IMAP account is often its email address. `newest` is when the newest message arrived, and `configured` names the account once it is in `[mail.accounts]`. Until Full Disk Access is granted, the log shows `cannot read the mail store yet, retrying`, and the daemon retries every 30 seconds.

4. Name the accounts clients may read under `[mail.accounts]`, then run `eventkit-bridge install` again:

   ```toml
   [mail.accounts]
   "1B9F0E52-6C3A-4D27-8E45-7A0B2C9D1E83" = "work"
   "6E2D8A17-4B90-4C3F-A1D5-9F8E7C6B5A42" = "gmail"
   ```

   Accounts not listed stay invisible to clients.

5. The first `PATCH /v1/mail/messages/{id}` (junk or not junk) shows the prompt "EventKitBridge wants to control Mail". Allow it. Mail is launched if it is not running. If you deny it, every junk request fails with `503` until you switch EventKitBridge on under System Settings, Privacy & Security, Automation, Mail. Reading mail does not need this permission.

The Envelope Index is an undocumented Apple format. The bridge was built against Mail data version `V10` on macOS 27, and a macOS release may change it; `/healthz` reports `mail schema changed` when a table or column the bridge reads disappears.

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

[mail]
exclude_mailboxes = ["Trash", "Deleted Items", "Deleted Messages", "Junk", "Junk Email", "Spam", "[Gmail]/Trash", "[Gmail]/Spam"]
# root = "/path/to/Mail/V10"

[mail.accounts]
"1B9F0E52-6C3A-4D27-8E45-7A0B2C9D1E83" = "work"
"6E2D8A17-4B90-4C3F-A1D5-9F8E7C6B5A42" = "gmail"

[auth]
issuer = "https://auth.example.com"
audience = "https://eventkit-bridge"
jwks_url = "https://auth.example.com/jwks.json"
# scope_prefix = "bridge:"
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
| `mail` | no | The `[mail]` table. Its presence turns mail on; without it the mail routes answer `404` and the mail store is never opened. |
| `mail.accounts` | no | Maps a Mail account uuid, a full UUID in either case, to the name clients see. A name follows the same rule as a place name. Two entries may not share a uuid or a name. Accounts not listed are invisible. Empty by default, which hides every account. |
| `mail.exclude_mailboxes` | no | Mailbox paths never shown, compared case-insensitively, such as `[Gmail]/Spam`. Defaults to the list in the example above; `[]` excludes nothing. An entry must not be blank. |
| `mail.root` | no | The Mail data directory to read. Defaults to the highest `~/Library/Mail/V<n>` that contains `MailData/Envelope Index`. Only useful for development. |
| `auth` | no | The `[auth]` table. Its presence turns the bearer-token gate on (see Authentication); without it no token is read. |
| `auth.issuer` | yes, in `[auth]` | The provider URL, compared exactly with the token's `iss`. Must not be blank or contain whitespace. |
| `auth.audience` | yes, in `[auth]` | The audience the token's `aud` must contain. Must not be blank or contain whitespace. |
| `auth.jwks_url` | yes, in `[auth]` | The absolute `http` or `https` URL of the provider's JWK set. There is no discovery. |
| `auth.scope_prefix` | no | Default `bridge:`. Put in front of every scope name, so `calendar.read` becomes `bridge:calendar.read`. May be empty. Only printable ASCII other than space, `"` and `\`. |

Unknown keys are rejected. The config is read once at startup; after changing it, run `eventkit-bridge install` again to restart the daemon.

`--check-config` prints the readable and writable calendars and lists and the place names with their radii. Place addresses are never printed or logged. It also prints `mail: off`, or `mail: on` with the configured mail accounts, the excluded mailboxes and the mail root; it never opens the mail store. Last, it prints `auth: off`, or `auth: on` with the issuer, audience, `jwks_url` and `scope_prefix`; it never fetches the keys.

#### Why places are named

A location trigger names a place from the config rather than taking an address from the client. CoreLocation, which `remindctl` uses to geocode, resolves street addresses but not store names: `Some Store` fails, its street address works. Named places keep geocoding predictable, and they keep home and shop addresses out of client logs and agent context. Give each place a street address, not a business name.

### Authentication

Authentication is optional. Without an `[auth]` table the bridge reads no token, and the network bind and the `Host` check are the whole access control.

With `[auth]`, every request except `GET /healthz` needs an OAuth 2.0 access token in the `Authorization: Bearer <token>` header; a request without one is refused with `401`. There is no setting that lets a request without a token through. The bridge is not an OAuth provider and has no login: an external OpenID provider issues the token to the client through the `client_credentials` grant, and the bridge checks it locally against the provider's published keys, without calling the provider per request.

```toml
[auth]
issuer = "https://auth.example.com"
audience = "https://eventkit-bridge"
jwks_url = "https://auth.example.com/jwks.json"
```

The token must be a JWT access token (RFC 9068) signed with `RS256`, with a `kid` and a `typ` of `at+jwt` or `application/at+jwt` in its header. The bridge checks the signature, `exp` and `nbf` (with 60 seconds of leeway), that `iss` equals `issuer`, that `aud` is present and contains `audience`, and that `client_id` or `sub` names the client. Scopes are read from `scp`, an array, and from `scope`, a space-separated string; both forms are accepted. Only the `Authorization` header is read, never a query parameter or a cookie.

To get a token, register a confidential client in the provider with the `client_credentials` grant, the bridge's audience and the scopes it needs, then request one with both `scope` and `audience`:

```sh
curl -u 'my-agent:<client secret>' https://auth.example.com/api/oauth2/token \
  -d grant_type=client_credentials \
  --data-urlencode 'scope=bridge:calendar.read bridge:reminders.read' \
  --data-urlencode 'audience=https://eventkit-bridge'
```

The `access_token` in the answer goes on every request to the bridge:

```sh
curl -H "Authorization: Bearer $TOKEN" http://100.64.0.1:8790/v1/calendars
```

Some providers, Authelia among them, issue a token without an `aud` claim when the request leaves out `audience`; the bridge refuses such a token with `401`. Request a new token before the old one expires; the bridge does not refresh tokens.

Each route needs one scope, `scope_prefix` followed by the name below:

| Scope | Routes |
| --- | --- |
| `calendar.read` | `GET /v1/calendars`, `GET /v1/events`, `GET /v1/events/{id}`, `GET /v1/free` |
| `calendar.write` | `POST /v1/events`, `PATCH` and `DELETE /v1/events/{id}` |
| `reminders.read` | `GET /v1/lists`, `GET /v1/places`, `GET /v1/reminders`, `GET /v1/reminders/{id}` |
| `reminders.write` | `POST /v1/reminders`, `PATCH` and `DELETE /v1/reminders/{id}` |
| `mail.read` | `GET /v1/mail/accounts`, `GET /v1/mail/messages`, `GET /v1/mail/messages/{id}` |
| `mail.junk` | `PATCH /v1/mail/messages/{id}` |
| none | `GET /healthz`, which needs no token at all |

A write scope does not include the read scope: a client that creates events and reads them asks for both `bridge:calendar.write` and `bridge:calendar.read`. `HEAD` needs the scope of `GET`. A path that does not exist, or a method a route does not accept, still needs a valid token, but no scope, before it answers `404` or `405`. The scopes decide which routes a client may call; which calendars, lists and mail accounts it may touch is still decided by the rest of the config.

| Case | Status | `WWW-Authenticate` | Body |
| --- | --- | --- | --- |
| No token | `401` | `Bearer` | `{"error":"missing bearer token"}` |
| A token that fails any check, including a second `Authorization` header or a scheme other than `Bearer` | `401` | `Bearer error="invalid_token"` | `{"error":"invalid bearer token"}` |
| A valid token without the route's scope | `403` | `Bearer error="insufficient_scope", scope="bridge:calendar.write"` | `{"error":"insufficient scope: bridge:calendar.write needed"}` |

The error never says which check failed; the log records only the kind of failure (see Logs).

The bridge fetches the provider's keys from `jwks_url` at startup, retries every 30 seconds until it succeeds, and refreshes them every 12 hours, or 5 minutes after a failed refresh. It keeps only `RSA` signing keys with a `kid`; a key set with none of them is treated as a failed fetch and the old keys stay. A token signed with a key the bridge does not know yet makes it fetch once more, at most once a minute, so a key rotated in at the provider works at once. The last good key set is saved to `~/.config/eventkit-bridge/jwks-cache.json` and loaded at startup, so a restart while the provider is down does not refuse every request. The file holds public keys only. The fetch trusts the certificates in the macOS keychain, so a provider behind a private CA the Mac already trusts works, and it ignores `HTTP_PROXY`. Each fetch gives up after 10 seconds and refuses a key set over 64 KiB.

To turn authentication on, give every client a token first, then add `[auth]` and run `eventkit-bridge install`; from then on a client without a token gets `401`.

### Command line

| Command | What it does |
| --- | --- |
| `eventkit-bridge` | Runs the daemon. This is what the LaunchAgent starts. |
| `eventkit-bridge install` | Validates the config, then writes and loads the LaunchAgent, replacing any existing one. |
| `eventkit-bridge uninstall` | Unloads and removes the LaunchAgent. The config is kept. |
| `eventkit-bridge --check-config` | Validates the config and exits `0` or `1`. |
| `eventkit-bridge --version` | Prints the version. |

`install` points the LaunchAgent at the real binary inside `EventKitBridge.app`, not at the Homebrew symlink, because the Calendars, Reminders and Full Disk Access permissions belong to the app bundle. It warns when the binary is not inside an `.app` bundle, since the permissions would then not survive an upgrade.

To remove the bridge completely, run `eventkit-bridge uninstall`, then `brew uninstall --cask --zap eventkit-bridge`, which also deletes the config directory (the config and, with `[auth]`, `jwks-cache.json`), the LaunchAgent plist and the log.

## HTTP API

The examples use `http://100.64.0.1:8790`. With `[auth]`, every example also needs `-H "Authorization: Bearer $TOKEN"` (see Authentication). Every response body is JSON. Every error is `{"error":"<message>"}` with the status codes below.

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

A body larger than 64 KiB is `413`. `POST` and `PATCH` must send `Content-Type: application/json`; any other content type, or none, is `415`. This stops a web page open in a browser on the tailnet from creating events or marking mail as junk with a cross-site form or `fetch` request.

### Status codes

| Status | Meaning |
| --- | --- |
| `400` | The request is invalid; the message names the parameter or field. |
| `401` | With `[auth]`: the bearer token is missing (`missing bearer token`) or fails a check (`invalid bearer token`). `WWW-Authenticate` says which. |
| `403` | The security policy refused the request, or, with `[auth]`, the token lacks the route's scope (`insufficient scope: <scope> needed`). |
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

The reminder routes follow the same rules as the calendar routes: the `Host` check, with `[auth]` a bearer token with the route's scope, `Content-Type: application/json` on `POST` and `PATCH`, the 64 KiB body limit, `{"error": ...}` errors, and `400` for unknown query parameters, unknown body fields and control characters other than newline and tab.

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
- `repeat` is `null`, one of `daily`, `weekly`, `biweekly`, `monthly` and `yearly`, or `every N days`, `every N weeks`, `every N months` or `every N years` for an interval of 2 to 999 units (every two weeks reads as `biweekly`). `remindctl` reports only a rule's frequency and interval, so a rule set elsewhere that also picks days, such as "monthly on the fourth weekday" or "every 2 months on the last weekend day", reads as its frequency and interval alone (`monthly`, `every 2 months`). A rule with any other frequency or interval is reported as `custom`.
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
| `repeat` | `daily`, `weekly`, `biweekly`, `monthly`, `yearly`, or `every N days`, `every N weeks`, `every N months` or `every N years` with N from 2 to 999, spelled exactly so (lowercase, one space, plural unit). Needs `due`. |
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

Reminder sections, tags, subtasks, smart lists, the Groceries list type and the Urgent toggle have no public EventKit API, so neither `remindctl` nor the bridge can read or set them. Alarms separate from the due time, URLs, repeat rules that pick days (such as "the fourth weekday of the month" or "the last weekend day"), moving a reminder between lists, and creating or renaming lists are not supported either.

## Mail API

The mail routes follow the same rules as the other routes: the `Host` check, with `[auth]` a bearer token with the route's scope, `{"error": ...}` errors, and `400` for unknown query parameters, repeated single-valued parameters and control characters. They accept `GET`, plus `PATCH` on `/v1/mail/messages/{id}`; any other method is `405`. Without a `[mail]` table every mail route answers `404 mail is off: add [mail] to the config`.

Mail reads do not wait for `ekctl` or `remindctl`: they open their own read-only connection to the Envelope Index per request. Junk requests do not wait for them either; they wait only for each other, one Mail call at a time.

### Types

An account:

```json
{"name":"work","type":"exchange","mailboxes":[{"path":"Inbox","total":812,"unread":3}]}
```

`type` is `exchange`, `imap` or `local`. `total` and `unread` are Mail's own counts for the mailbox.

A message summary:

```json
{
  "id": 383621,
  "account": "work",
  "mailbox": "Inbox",
  "date": "2026-10-05T15:35:13+02:00",
  "from": {"name": "A Person", "address": "a@example.com"},
  "to": [{"name": null, "address": "me@example.com"}],
  "subject": "Quarterly report",
  "summary": "The numbers for the third quarter",
  "read": false,
  "flagged": false,
  "has_body": true
}
```

A message is a summary plus:

```json
{
  "cc": [{"name": "Carol", "address": "carol@example.com"}],
  "body": "Plain text of the message",
  "body_truncated": false,
  "partial": false,
  "attachments": [{"name": "report.pdf", "content_type": "application/pdf", "size": 120334}]
}
```

- `id` is Mail's row id for the message.
- `date` is when the message was received, RFC 3339 in the Mac's time zone.
- `from`, `subject` and `summary` are `null` when Mail stored none, and a name is `null` when the address has no display name. Mail keeps a `summary` for only some messages.
- `has_body` is false when Mail has no file for the message; `body` is then `null`.
- `body` is the first `text/plain` part, or else the first `text/html` part converted to plain text. It is cut at 100 000 characters, with `body_truncated: true`.
- `partial` is true when Mail holds only part of the message, such as the headers and the start of the body. `body` is then whatever could be recovered.
- `attachments` lists names, content types and sizes in bytes; `name` is `null` when the message gives none. Attachment contents are never served.

### `GET /v1/mail/accounts`

Lists the configured accounts that have mailboxes, by name, with their mailboxes by path. Excluded mailboxes are left out.

```sh
curl http://100.64.0.1:8790/v1/mail/accounts
```

```json
{"accounts":[{"name":"gmail","type":"imap","mailboxes":[{"path":"INBOX","total":402,"unread":5},{"path":"[Gmail]/All Mail","total":40211,"unread":12}]},{"name":"work","type":"exchange","mailboxes":[{"path":"Inbox","total":812,"unread":3}]}]}
```

### `GET /v1/mail/messages`

Lists message summaries, newest first by the time they were received.

| Parameter | Meaning |
| --- | --- |
| `account` | Optional and repeatable. A configured account name. Without it, every configured account is read. |
| `mailbox` | Optional, at most 1 000 characters. A mailbox path within those accounts, such as `Inbox` or `[Gmail]/All Mail`, compared case-insensitively. |
| `since`, `until` | Optional RFC 3339 timestamps. Messages received at or after `since` and before `until`. `until` must be after `since`. |
| `q` | Optional, at most 500 characters. A case-insensitive substring of the subject, the sender's name or address, a recipient's address, or Mail's summary. Case folding covers every script, not only ASCII. |
| `unread` | `true` lists unread messages only; `false`, the default, lists both. |
| `limit` | The page size, 1 to 100, default 25. |
| `cursor` | The `next_cursor` of the previous page. |

```sh
curl 'http://100.64.0.1:8790/v1/mail/messages?account=work&mailbox=Inbox&since=2026-10-01T00:00:00%2B02:00&q=report&limit=2'
```

```json
{"messages":[{"id":383621,"account":"work","mailbox":"Inbox","date":"2026-10-05T15:35:13+02:00","from":{"name":"A Person","address":"a@example.com"},"to":[{"name":null,"address":"me@example.com"}],"subject":"Quarterly report","summary":"The numbers for the third quarter","read":false,"flagged":false,"has_body":true}],"next_cursor":null}
```

- `next_cursor` is `null` on the last page. Pass it back as `cursor` with the same filters to get the next page. A cursor that is not one the bridge returned is `400`; a cursor reused with different filters is not detected and simply continues from that position.
- Deleted messages, messages in excluded mailboxes and messages in accounts not in `[mail.accounts]` never appear.
- A message in several mailboxes, such as a Gmail message with labels, appears once per mailbox, `[Gmail]/All Mail` included.
- `q` does not search message bodies. It matches Mail's stored summary, which is empty for many messages.
- An account name that is not configured is `400 unknown mail account "<name>"`, and a mailbox that is not a visible mailbox of the chosen accounts is `400 unknown mailbox "<path>"`.
- The bounds are `since` and `until`, not `from` and `to`; `from` is `400`.

### `GET /v1/mail/messages/{id}`

Returns one message with its body.

```sh
curl http://100.64.0.1:8790/v1/mail/messages/383621
```

An id that is not a positive integer is `400`. A message that is deleted, in an excluded mailbox, in an account not in `[mail.accounts]`, or does not exist is `404 message not found`; the four cases look the same.

### `PATCH /v1/mail/messages/{id}`

Marks a message as junk and moves it to its account's junk mailbox, or marks it as not junk and moves it back to the inbox. Mail.app does the move and syncs it to the server, so Gmail and Exchange learn from it the same way as from a click in Mail. The body is `{"junk": true}` or `{"junk": false}`, sent as `application/json`; `junk` is required and is the only field.

```sh
curl -X PATCH -H 'content-type: application/json' -d '{"junk": false}' http://100.64.0.1:8790/v1/mail/messages/383621
```

```json
{"id":383622,"account":"work","mailbox":"Inbox","junk":false}
```

- The message must be visible, as for `GET`; otherwise it is `404 message not found`.
- The junk mailbox is the first of the account's mailboxes named `[Gmail]/Spam`, `Junk Email`, `Junk E-mail`, `Junk` or `Spam`, in that order, compared case-insensitively. The inbox is the mailbox named `INBOX`, in any case. An account without one is `409 account has no junk mailbox` or `409 account has no inbox`.
- Mail gives a moved message a new id. `id` is that new id, or the same id when the message was already in the target mailbox. It is `null` when the moved copy did not show up within about 5 seconds, when the message has no Message-ID to find the copy by, or when the target mailbox is in `mail.exclude_mailboxes`. The default `exclude_mailboxes` hides every junk mailbox except `Junk E-mail`, so with it a message marked as junk usually gets `id: null`, and a message in junk is not visible and cannot be marked as not junk; remove the junk mailbox from `exclude_mailboxes` to rescue mail from it.
- `account` is the configured account name, `mailbox` the target mailbox path and `junk` the requested value.
- One message per request. Junk requests run one at a time.

### Mail status codes

| Status | Meaning |
| --- | --- |
| `400` | The request is invalid; the message names the parameter. |
| `401` | With `[auth]`: the bearer token is missing or invalid. |
| `403` | With `[auth]`: the token lacks `mail.read`, or `mail.junk` for a `PATCH`. |
| `404` | Mail is off, the message is not visible, the route does not exist, or Mail could not find the message to mark (it moved or was deleted in the meantime). |
| `405` | A method other than `GET`, or other than `GET` and `PATCH` on `/v1/mail/messages/{id}`. |
| `409` | The account has no junk mailbox, or no inbox. |
| `413` | A `PATCH` body larger than 64 KiB. |
| `415` | A `PATCH` body that is not `application/json`. |
| `500` | A query against the Envelope Index failed, or a message file could not be read. The error names only the kind of failure, never the file path. |
| `502` | `Mail failed`: `osascript` could not start, Mail returned an error while marking the message, or `osascript` printed output the bridge does not understand. |
| `503` | The mail store could not be opened, which is how a missing Full Disk Access grant shows up; or `Mail automation not permitted`, when EventKitBridge may not control Mail. |
| `504` | `Mail did not answer` within 30 seconds. |

### Not available

Sending, replying, flagging, marking read, deleting mail, and moving it anywhere but junk and the inbox; marking several messages in one request; attachment contents; full-text search over message bodies; and de-duplicating a message that appears in several mailboxes.

## Health check

```sh
curl http://100.64.0.1:8790/healthz
```

When the bridge can read every configured calendar, it answers `200`:

```json
{"status":"ok","version":"0.8.0","calendars":2,"lists":1,"mail_accounts":2,"newest_message_age_s":95,"auth":{"jwks_keys":2,"jwks_age_s":3120}}
```

`calendars` is the number of readable calendars that exist. When reminder lists are configured, the check also runs `remindctl`, and `lists` is the number of readable lists that exist; without lists, `lists` is left out and `remindctl` is not run. Unlike a missing calendar, a configured list that no longer exists does not make the check degraded; it only lowers `lists`, so compare `lists` with the number of lists in your config.

When `[mail]` is present, the check also opens the mail store, checks that every table and column the bridge reads exists, and checks that every account in `[mail.accounts]` has mailboxes. `mail_accounts` is the number of configured accounts, and `newest_message_age_s` is how many seconds ago the newest visible message arrived, or `null` when there is none. A value that keeps growing means Mail stopped syncing. Without `[mail]`, both fields are left out.

When `[auth]` is present, `auth.jwks_keys` is the number of usable keys the bridge holds, and `auth.jwks_age_s` is how many seconds ago they were fetched from the provider, or `null` when no keys are loaded. Keys loaded from `jwks-cache.json` keep the age of their original fetch. Without `[auth]`, `auth` is left out. `/healthz` never needs a token.

Otherwise it answers `503`:

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
| `mail no access` | `[mail]` is present and the mail store cannot be opened or read. This is how a missing Full Disk Access grant shows up. |
| `mail schema changed` | A table or column the bridge reads is missing from the Envelope Index, most likely after a macOS update. |
| `mail account missing` | An account in `[mail.accounts]` has no mailboxes in Mail's store. |
| `auth jwks unavailable` | `[auth]` is present and the bridge holds no keys at all, neither fetched from `jwks_url` nor loaded from `jwks-cache.json`, so every token is refused. Old keys do not count as degraded. |

A calendar, reminder or mail problem is reported before `auth jwks unavailable`. `timeout` covers `remindctl` as well. The check runs `ekctl` (and `remindctl`, and the mail check) at most once every 10 seconds and reuses the result in between, so it is safe to poll.

## Upgrades

There is usually nothing to do. `brew upgrade --cask eventkit-bridge` replaces the app; the running daemon notices within a couple of seconds that its binary changed, finishes the requests in flight and exits, and launchd starts the new version. The Calendars, Reminders and Full Disk Access permissions belong to the app bundle and carry over.

Upgrading to 0.8.0 with an `[auth]` table: delete the `required` line from `[auth]` first and run `eventkit-bridge install`. 0.8.0 no longer knows the key, so a config that still has it fails to load with an unknown-key error naming `required`. On 0.7.0 a missing `required` means `true`, so deleting the line changes nothing there; if it was `required = false`, make sure every client sends a token before deleting it, because 0.8.0 refuses a request without one with `401`.

## Logs

The daemon logs to `~/Library/Logs/eventkit-bridge.log`. Each request produces one line with the method, the route template, the status, the duration and, when `ekctl` or `remindctl` ran, each subcommand with its exit code. A successful mail read also logs how many rows it returned, and a junk request for a visible message logs the account name and the requested junk value, whatever its outcome. With `[auth]`, the line ends with `client=` and the token's `client_id` (or `sub`) once the token is valid, also when it is refused with `403` for a missing scope. A request refused with `401` has no `client`, and without `[auth]` the field is left out:

```
2026-10-05T09:12:40.881207Z  INFO eventkit_bridge::server: request method=PATCH route=/v1/events/{id} status=403 duration_ms=212 ekctl="show event=0"
2026-10-05T09:13:02.104377Z  INFO eventkit_bridge::server: request method=PATCH route=/v1/reminders/{id} status=200 duration_ms=164 remindctl="info=0, edit=0"
2026-10-05T09:13:40.402118Z  INFO eventkit_bridge::server: request method=GET route=/v1/mail/messages status=200 duration_ms=18 rows=25
2026-10-05T09:14:05.611904Z  INFO eventkit_bridge::server: request method=PATCH route=/v1/mail/messages/{id} status=200 duration_ms=1240 account="work" junk=true
2026-10-05T09:15:21.030455Z  INFO eventkit_bridge::server: request method=GET route=/v1/calendars status=200 duration_ms=95 client=my-agent
```

A refused token also logs `token refused` with its kind (`missing`, `malformed`, `unknown key`, `invalid` or `insufficient scope`) and nothing else. At startup, `auth on` logs the issuer, audience and `scope_prefix`, and `jwks loaded` the number of keys and whether they came from the `file` or the `provider`; a failed fetch logs `cannot fetch the jwks` with the HTTP status or the kind of error, never the body.

Policy refusals log their reason. Event titles, notes, locations, URLs and attendees, reminder titles and notes, place addresses and coordinates, and mail addresses, names, subjects, summaries, bodies, attachment names, Message-IDs and mailbox paths of junk requests are never logged; of a failed Mail call, only its error code is logged. The only calendar, reminder and mail details in the log are the startup listing of calendar ids, titles and accounts, reminder list ids and titles, place names with their radii, and mail account uuids with their type, description and counts. Bearer tokens, the `Authorization` header and every token claim other than the client id are never logged. The description of an IMAP account is often its email address; it appears once, in the startup listing.

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
- **`/healthz` says `mail no access`, or mail requests fail with `503`.** The bridge has no Full Disk Access. Check System Settings, Privacy & Security, Full Disk Access, and make sure `EventKitBridge` is listed and switched on, then run `eventkit-bridge install` to restart the daemon.
- **Junk requests fail with `503 Mail automation not permitted`.** Mail answered with error `-1743`: EventKitBridge is not allowed to control Mail. Open System Settings, Privacy & Security, Automation, expand EventKitBridge, and switch on Mail. If EventKitBridge is not listed or the prompt never appeared, reset the permission to get a fresh prompt on the next junk request:

  ```sh
  tccutil reset AppleEvents dev.pkarpovich.eventkit-bridge
  ```

- **`/healthz` says `mail account missing`.** An account in `[mail.accounts]` was removed from Mail or re-added under a new uuid. Look up the current uuids in the startup listing in the log and update the config.
- **`/healthz` says `mail schema changed`.** A macOS update changed the Envelope Index. Mail reads stay unreliable until the bridge is updated for the new format; remove `[mail]` to keep the rest of the bridge healthy meanwhile.
- **`/healthz` says `auth jwks unavailable`.** The bridge has never fetched the provider's keys and has no `jwks-cache.json` to fall back on, so every token is refused. Look for `cannot fetch the jwks` in the log: a `status` means the provider answered with an error, so check `jwks_url` in a browser; `timed out` or `transport error` means the Mac cannot reach the provider or does not trust its certificate. `no usable keys` means the key set has no `RSA` signing key with a `kid`. `body too large` means the key set is over 64 KiB, and `not a jwk set` means `jwks_url` did not return a `{"keys": [...]}` document, which usually means it points at a login or discovery page. The bridge retries every 30 seconds, and the check turns healthy with the first successful fetch.
- **Every request with a token gets `401 invalid bearer token`, and the log says `token refused kind=invalid`.** Most often the token has no `aud`: some providers, Authelia among them, issue a token without one when the token request leaves out `audience`. Request the token with `audience` set to the `audience` in `[auth]`, and check that the client is allowed that audience in the provider. Decode the token's middle segment (base64url) to compare its `iss` and `aud` with the config; `iss` must match `issuer` exactly, including a trailing slash.
- **The bridge is unreachable after upgrading to 0.8.0, and the log says ``unknown field `required` ``.** 0.8.0 removed `required` from `[auth]`, and launchd keeps restarting a daemon that cannot load its config. Delete the line from `~/.config/eventkit-bridge/config.toml`, check that every client sends a token (a request without one is now `401`), and run `eventkit-bridge install`.
- **The bridge is unreachable after a reboot.** It is a LaunchAgent, so it runs only in your login session. With FileVault on, it starts only after you log in following a reboot. If it starts before tailscale is up, the bind fails, and launchd keeps restarting it until the address exists.
- **`install` warns that the program is not inside an `.app` bundle.** You ran a binary from somewhere other than the installed app. The Calendars, Reminders and Full Disk Access permissions are tied to `EventKitBridge.app` and would not survive an upgrade; run `install` from the Homebrew-installed `eventkit-bridge`.

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

The crate builds and its tests pass on macOS and Linux; no test runs `ekctl`, `remindctl`, `osascript` or `launchctl` for real, talks to Mail, or reads a real `~/Library/Mail`: mail tests build a fixture store from `fixtures/mail_schema.sql`.

```sh
mise run check
```

runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. CI also runs `shellcheck scripts/*.sh`.

Do not run the daemon, `ekctl` or `remindctl` from a terminal against your real calendars, reminders or mail: TCC would grant access, including the Automation permission to control Mail, to the terminal app, not to EventKitBridge, and reading mail would need Full Disk Access for the terminal. Test on the Mac with the bundled app started by the LaunchAgent.

## Credits

The bridge runs [`ekctl`](https://github.com/schappim/ekctl) by schappim and [`remindctl`](https://github.com/openclaw/remindctl) by OpenClaw, both released under the MIT License. The app bundle includes their licenses as `Contents/Resources/ekctl-LICENSE.txt` and `Contents/Resources/remindctl-LICENSE.txt`.

## License

MIT. See [LICENSE](LICENSE).
