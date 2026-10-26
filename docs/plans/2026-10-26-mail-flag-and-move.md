# Flag a message and move it between mailboxes

## Overview

eventkit-bridge 0.9.0 adds two mail writes next to junk / not junk: set or clear a message's flag in one of Mail's seven colours, and move a message from one mailbox to another inside its account. Both go through Mail.app over Apple Events, as the junk write does, so Mail syncs them to the server.

Both writes are generic on purpose. The bridge does not know what a colour means or what "archive" is: a client decides that orange means "the user should delete this", or that archiving a Gmail message is a move from `INBOX` to `[Gmail]/All Mail`. The bridge owns only the policy, which mailboxes a move may touch.

Reads change too. A message reports its flag colour and, for Gmail, the labels it carries, and `GET /v1/mail/messages` filters by colour. Gmail labels are a prerequisite for the move: today a Gmail inbox message reads as `[Gmail]/All Mail` and `mailbox=INBOX` finds nothing (see Context).

```
client --PUT/DELETE /v1/mail/messages/{id}/flag--> eventkit-bridge --osascript--> Mail.app --> server
client --POST /v1/mail/messages/{id}/move--------> eventkit-bridge --osascript--> Mail.app --> server
```

### Non-goals

- Deleting, sending, replying, marking read or unread. There is no delete route.
- A move into or out of a mailbox listed in `no_move_mailboxes` or hidden by `exclude_mailboxes`. The bridge has no built-in list of trash mailboxes: keeping trash out of moves is the config's job, through these two keys.
- Moving a message to another account.
- Giving colours a meaning in the bridge. A colour is a name and nothing else.
- Replacing junk / not junk. A move into or out of a junk mailbox does not set the junk status and does not train the provider; clients keep using `PATCH /v1/mail/messages/{id}` for that.
- Batch requests. One message per request.
- A default for `no_move_mailboxes`. It is empty unless the user lists mailboxes.

### Rejected alternatives

- **Dedicated archive / unarchive routes with per-account archive mailbox detection.** Rejected for one generic move: Gmail has no archive mailbox (archive is removing the `INBOX` label), and a client already sees every mailbox through `GET /v1/mail/accounts`.
- **One fixed "delete" flag (orange only).** Rejected for a colour argument: the client keeps a meaning per colour (delete, unsubscribe, ...) without a bridge change.
- **Reusing `exclude_mailboxes` as the move policy.** Rejected: a user may keep Junk and Spam visible to read and rescue spam while still keeping moves out of them, so visible must not mean movable.
- **A built-in list of trash mailbox names that is always closed to moves.** Rejected: it would be a second source for one policy, and a name list still misses a localised trash name, which the config has to close anyway.
- **Mailboxes in `no_move_mailboxes` by Envelope Index `ROWID`.** Rejected: the id is local to Mail's index and opaque in a config file; a path is what `exclude_mailboxes`, AppleScript and the API already use.
- **Flag and move as new fields on `PATCH /v1/mail/messages/{id}`.** Rejected: `require_bearer` checks the scope from the method and route template before the body is read, so one route can carry only one scope; separate routes give `mail.flag` and `mail.move` their own scopes.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every Rust file under `src/` follows it.

## Context (verified on 2026-10-26, macOS 27.0, Mail data version V10, against a Gmail IMAP account)

- **Flag through AppleScript.** `flag index of m` reads `-1` when unflagged, otherwise the colour index. `set flag index of m to N` flags the message and sets the colour in one step, also on an unflagged message. `set flagged status of m to false` clears the flag, after which `flag index` reads `-1`. Neither changes the message id. Verified with `1` (an orange flag set in Mail reads `1`) and `4` (blue). The colour order is Mail's menu order: 0 red, 1 orange, 2 yellow, 3 green, 4 blue, 5 purple, 6 gray; indexes 0, 2, 3, 5 and 6 are not individually verified.
- **Flag colour in the Envelope Index.** `messages.flagged` is the flag. `messages.flags` (`INTEGER NOT NULL DEFAULT 0` in the real schema) carries the colour in bits 39-41, `(flags >> 39) & 7`: an orange flag reads colour 1, blue colour 4, and older red flags colour 0, while the low bits stay the same across colour changes. Clearing the flag leaves the colour bits set, so a colour means something only when `flagged = 1`. `flagged` equals `(flags >> 4) & 1` on every row of the test index.
- **The colour syncs.** An orange flag set on one Mac showed orange in Mail on a second Mac on the same Gmail account. iCloud and Exchange colour sync is not verified.
- **Gmail labels.** In V10 a Gmail message is one `messages` row whose `mailbox` is `[Gmail]/All Mail`. `INBOX`, user labels, `[Gmail]/Important`, `[Gmail]/Starred` and `[Gmail]/Sent Mail` are rows in `labels (message_id, mailbox_id)`, primary key `(message_id, mailbox_id)`, `WITHOUT ROWID`, with an index on `mailbox_id`; those mailboxes have `mailboxes.source` set to the All Mail row. `[Gmail]/Spam`, `[Gmail]/Trash` and `[Gmail]/Drafts` are ordinary mailboxes. In the test index every Gmail inbox message has source `[Gmail]/All Mail` and none has source `INBOX`, so today's `mailbox IN rarray(:mailboxes)` filter returns 0 for `mailbox=INBOX`. Exchange (`ews://`) accounts have no `labels` rows. The fixture models a Gmail message as two separate rows (383621 in `INBOX`, 383622 in All Mail), which is why no test noticed.
- **AppleScript sees labels as mailboxes.** `messages of mailbox "INBOX" of account id <uuid> whose message id is <rfc>` and the same query in `[Gmail]/All Mail` both return the same id, and `first message of mailbox "INBOX" ... whose id is <id>` finds a message whose source is All Mail.
- **Gmail archive and back.** `move m to mailbox "[Gmail]/All Mail"` with `m` addressed through `mailbox "INBOX"` removed the `INBOX` label within 2 s; Mail gave the row a new `ROWID` and kept the flag. `move m to mailbox "INBOX"` with `m` addressed through `[Gmail]/All Mail` added the label back within 2 s and kept the `ROWID`. A move between two user labels is not verified.
- **Exchange and iCloud archive** are ordinary mailboxes named `Archive`. A move between ordinary mailboxes behaves like the junk move: a new id, found by `Message-ID` in the target.
- **Code to build on.** Routes in `routes()` and layers in `router()` (`src/server.rs`); the junk handler `mark_mail_junk` / `move_junk`, `mail_script_failure` and `mail_failure` (`src/server.rs`); body and query parsing `mail_junk_body`, `mail_messages_query`, `Params`, `check_text` (`src/request.rs`); `MESSAGES_SQL`, `MESSAGE_SQL`, `MESSAGE_MAILBOX_SQL`, `RECIPIENTS_SQL`, `SCHEMA`, `MessageQuery`, `place()`, `target()`, `mailbox_records()`, `select()` (`src/mail/store.rs`); the junk `SCRIPT`, `Runner`, `parse_moved`, `classify` (`src/mail/script.rs`); `RawMail`, `MailConfig`, `is_excluded`, `parse_mail` (`src/config.rs`); `SCOPE_TABLE`, `scope_for` (`src/auth/mod.rs`); the junk test helpers `osascript_fake()`, `junk_app()`, `ALL_SCOPES`, `scoped_cases()`, `mail_request_log_carries_rows_but_no_content` (`src/server.rs`); `Fixture`, `Row` (`src/mail/fixture.rs`); `fixtures/mail_schema.sql`.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task), as in the repo's earlier plans.
- Complete each task fully before the next; small, focused changes.
- **Every task includes new or updated tests** for the code it changes, covering success and error paths.
- **All tests must pass before the next task starts** (`mise run check`).
- No test runs the real `osascript` or talks to Mail. Tests use a fake `osascript` in the style of `osascript_fake()` and `src/fake_ekctl.rs`.
- Update this plan when scope changes during implementation.

## Code-Quality Rules (verify before marking each task complete)

The `rust-style` skill has no `## Hard rules` block; its rule headings and the repo's CLAUDE.md rules are the gate.

`rust-style`:
- Control flow: use `for` loops, not iterator chains.
- Early returns: use `let ... else`.
- Pattern matching: minimize `if let`; never use wildcard matches; avoid the `matches!` macro.
- Variable naming: shadow, don't rename.
- Comments: don't write them (`///` on `pub` items only, per CLAUDE.md).
- Type safety: prefer newtypes over strings; prefer strongly-typed enums over bools.
- Destructuring: always use explicit destructuring.

Repo (CLAUDE.md):
- No `#[allow]`; no function with more than 3 parameters (`clippy.toml`), pass a struct.
- Tests inline in `#[cfg(test)] mod tests`; HTTP tests through `oneshot`.
- Only `src/mail/script.rs` builds `osascript` argv; scripts are compile-time constants with an `on run argv` handler, values passed as argv, never interpolated.
- Mail SQL is parameterised; the Envelope Index is opened read-only and never `immutable`.
- Every route in `router()` other than `/healthz` has a row in the scope table.

Per-task gate: `mise run check` green (`cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`), and `git diff` adds no `#[allow`, no `//` comment lines in `src/`, no `matches!`.

## Testing Strategy

- **Unit tests:** required for every task, inline in the module they test.
- **HTTP tests:** `tower::ServiceExt::oneshot` against `router()` with the fake `osascript` and the fixture store.
- **E2E tests:** the project has none.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕ prefix, blockers with ⚠️ prefix.
- Keep this plan in sync with the actual work.

## Solution Overview

- Reads become label- and colour-aware in SQL only: the label membership and the colour come from the Envelope Index, the same read-only way every mail read works today.
- Writes reuse the junk machinery: the same `Runner`, lock, deadline, error mapping and new-id lookup, with two new compile-time scripts. The bridge resolves and checks every mailbox path from the Envelope Index before Mail is called, so Mail is never asked to touch a closed mailbox.
- The move policy is one config list, matched by path in every account.

## Technical Details

### Reads

`MessageSummary` (and so `Message`) gains:
- `flag`: `red`, `orange`, `yellow`, `green`, `blue`, `purple` or `gray` when `flagged = 1`, otherwise `null`. `flagged` stays.
- `labels`: the decoded paths of the message's visible label mailboxes, sorted; `[]` when there are none, which is every Exchange and iCloud message.

Visibility does not change: a message is visible only when its source mailbox is visible (configured account, not excluded, not deleted). Labels add membership for the `mailbox=` filter, the `labels` field and the move's `from`, never visibility. With `[Gmail]/All Mail` excluded, Gmail mail stays invisible, as it is today; this keeps every reader that resolves a row through its source mailbox (`messages()`, `message()`, `place()`, `locate()` for the `.emlx`, `NEWEST_SQL`) correct without change. `mailbox` stays the source path.

A new `FlagColor` enum (seven variants, index 0-6, serde lowercase names) lives in `src/mail/mod.rs` next to `MessageId`, with conversions from the index and from the name.

`GET /v1/mail/messages`:
- `mailbox=<path>` matches a message whose source is that mailbox or that carries it as a label, so `mailbox=INBOX` on Gmail returns the inbox.
- New `flag=<colour>` returns only messages with `flagged = 1` and that colour. Unknown colour: `400` `` `flag` must be one of red, orange, yellow, green, blue, purple, gray ``. Given twice: the existing `400`.
- Without `mailbox`, a Gmail message appears once, whatever its labels.

SQL, all parameterised:
- `MESSAGES_SQL` and `MESSAGE_SQL` select `CASE WHEN m.flagged = 1 THEN (m.flags >> 39) & 7 END`.
- In `MESSAGES_SQL` only, `m.mailbox IN rarray(:mailboxes)` becomes `m.mailbox IN rarray(:visible) AND (m.mailbox IN rarray(:mailboxes) OR EXISTS (SELECT 1 FROM labels l WHERE l.message_id = m.ROWID AND l.mailbox_id IN rarray(:mailboxes)))`, where `:visible` is every visible mailbox id and `:mailboxes` the selection `select()` returns today (the filtered mailboxes, or every visible one without `mailbox`). `MESSAGE_SQL` and `MESSAGE_MAILBOX_SQL` keep their source-only predicate.
- `MESSAGES_SQL` adds `AND (:flag IS NULL OR (m.flagged = 1 AND (m.flags >> 39) & 7 = :flag))`.
- A new `LABELS_SQL` runs per page and per single message, like `RECIPIENTS_SQL`: `SELECT message_id, mailbox_id FROM labels WHERE message_id IN rarray(:messages) AND mailbox_id IN rarray(:visible) ORDER BY message_id`. It binds `:visible`, never the `mailbox=` selection, so `labels` is the same whether a message was found with a filter, without one, or by id. Mailbox ids map to paths through the mailbox records already loaded.

Schema and fixture:
- `SCHEMA` gains `flags` under `messages` and a `("labels", &["message_id", "mailbox_id"])` entry.
- `fixtures/mail_schema.sql` gains `flags INTEGER NOT NULL DEFAULT 0` on `messages`, the `labels` table as in Context (foreign keys to `messages(ROWID)` and `mailboxes(ROWID)` with `ON DELETE CASCADE`) and `labels_mailbox_id_index`, without the real index's triggers.
- The fixture gains a Gmail message in `GMAIL_ALL` with a `GMAIL_INBOX` label, a second label on a new user-label mailbox (for example `Receipts`), and an orange flag (`flags = (1 << 39) | (1 << 4)`, `flagged = 1`), and one with orange colour bits and `flagged = 0`. The existing two-row Gmail message stays.

### Config

`[mail]` gains `no_move_mailboxes`, an optional array of mailbox paths, default empty. A path matches case-insensitively on the whole path, in every account, as `exclude_mailboxes` does. A listed mailbox stays readable. `RawMail` keeps `deny_unknown_fields`; `MailConfig` gains the list and `is_no_move(path)` next to `is_excluded`.

### Flag routes

- `PUT /v1/mail/messages/{id}/flag` with `{"color": "<colour>"}`: `color` is required, the only field, one of the seven names; anything else is `400` under the existing body rules. `require_json` applies.
- `DELETE /v1/mail/messages/{id}/flag`, no body.
- The message must be visible, otherwise `404 message not found`; mail off is the existing `404`. The message is addressed through the source mailbox `place()` returns.
- `200` `{"id": 383622, "flag": "orange"}` for `PUT`, `{"id": 383622, "flag": null}` for `DELETE`. The id never changes. Clearing an unflagged message, or setting the colour it already has, answers `200`.
- Scope `mail.flag` on both rows of `SCOPE_TABLE`.

### Move route

`POST /v1/mail/messages/{id}/move` with `{"from": "<path>", "to": "<path>"}`: both required strings and the only fields, each non-empty, at most 1000 characters, no control characters (`check_text`). `require_json` applies. Scope `mail.move`.

A new store function next to `place()` resolves the move from the Envelope Index, in this order:
1. The message must be visible, otherwise `404 message not found`.
2. `from` and `to` are matched case-insensitively on the whole path against the mailboxes of the message's account; the stored case is what goes to Mail.
3. If `from` or `to` is in `no_move_mailboxes` or `exclude_mailboxes`: `403 mailbox is closed to moves`. Checked before membership so the answer does not depend on where the message is.
4. `from` must be the message's source mailbox or one of its label mailboxes, otherwise `409 message is not in the from mailbox`.
5. `to` must exist in the account, otherwise `409 account has no to mailbox`.
6. A move takes the message out of `from`, except when `from` is its source mailbox: a Gmail message never leaves `[Gmail]/All Mail`, so a move from there only adds `to` as a label. The message is therefore already there only when `from` equals `to`, or when `from` is the source mailbox and `to` is a label the message already carries (All Mail -> `INBOX` for a message already in the inbox). Every other case is a real move, including a move from a label to the source (the Gmail archive, `INBOX` -> `[Gmail]/All Mail`, which removes the `INBOX` label) and from a label to a label the message already carries (which still removes `from`).

It returns one enum: `Move` with the account and the stored `from` and `to` paths, `AlreadyThere` with the account and the stored `to` path, or a refusal (`Closed`, `NotInFrom`, `NoTarget`). The handler answers `AlreadyThere` with `200`, the same id, and no Mail call: a Gmail move from All Mail into a label the message already carries would otherwise keep its `ROWID`, find no new id, and hold the Mail write lock for the 5 s wait before answering `id: null`. A label-to-label move into a label the message already carries is unverified on Mail; it may answer `id: null` after the wait, which is the documented "find it with a search" answer.

Otherwise one `osascript` call moves the message and waits for the copy, and the route answers `200` `{"id": 383622, "account": "gmail", "mailbox": "[Gmail]/All Mail"}`: `id` is the id after the move or `null` when the copy did not show up within 5 s, `mailbox` the stored `to` path.

### The Mail runner

`src/mail/script.rs` gains two compile-time scripts and two `Runner` methods, each taking a struct:
- `Runner::flag(FlagWrite { account, source, id, color })`, where `color` is `Some(FlagColor)` or `None` for clear. Argv after the script: account uuid, source path, message id, then the colour index or `off`. The script finds the message as the junk script does, then sets `flag index` to the index, or `flagged status` to false for `off`, and prints the message id. Output that is not that id is `UnexpectedOutput`.
- `Runner::move_message(MailMove { account, from, id, to })`. Argv: account uuid, from path, message id, to path. The script is the junk script minus the `junk mail status` line and minus its `sourcePath is targetPath` short cut (the store's `AlreadyThere` answers that case before Mail): find the message in `from`, read its `Message-ID`, record the ids already in `to` with that `Message-ID`, `move` it, poll `to` every 0.2 s for up to 5 s for a new id. Output parsing is `parse_moved`.
- Both use the existing Mail write `tokio::sync::Mutex` (so junk, flag and move are serialized), the 30 s process deadline, `with timeout of 20 seconds`, and the junk error mapping: `-1743` -> `503`, `-1728` -> `404`, `-1712` or the deadline -> `504`, anything else -> `502`, never the stderr text.

### Logging

The request line carries the route template, status, duration, the account name, and for a flag write the colour name or `off`. Never logged: mailbox paths (`from`, `to`, labels), Message-IDs, subjects, addresses, `osascript` stderr beyond its error code.

### Security model text

- CLAUDE.md, every place that says junk is the only mail write:
  - line 3 (the intro sentence "makes one mail write, junk or not junk") names the three writes;
  - the `osascript` argv rule lists the values passed: account uuid, mailbox paths (source, `from`, `to`), message id, the junk value, the colour index or `off`;
  - the design-doc list gains `docs/plans/completed/2026-10-26-mail-flag-and-move.md`;
  - the whole "single mail write" bullet, including its second sentence "`PATCH /v1/mail/messages/{id}` moves ... and nothing else: no sending, deleting, flagging, marking read or other moves", is replaced by the text below.
- That bullet: "The single mail write is junk / not junk" becomes "Mail writes are junk / not junk, flag / unflag, and a move between two mailboxes of one account, performed by Mail.app over Apple Events. A move is refused for any mailbox in `no_move_mailboxes` or `exclude_mailboxes`; the bridge has no built-in trash list, so keeping trash mailboxes out of moves is the config's job. No sending, no delete route, no marking read." The logging rule gains the flag colour.
- `Info.plist.template` `NSAppleEventsUsageDescription`: "EventKitBridge asks Mail to move a message or change its flag when a client requests it."

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests and docs in this repository.
- **Post-Completion** (no checkboxes): checks on the real Mac, Authelia and the user's config, the turtle-hub consumer.

## Implementation Steps

### Task 1: Flag colour and Gmail labels in reads

**Files:**
- Modify: `fixtures/mail_schema.sql`
- Modify: `src/mail/fixture.rs`
- Modify: `src/mail/mod.rs`
- Modify: `src/mail/store.rs`
- Modify: `src/request.rs` (the `MessageQuery` literal in `mail_messages_query` and in its test get `flag: None`; Task 2 fills it)

- [x] `fixtures/mail_schema.sql` and `SCHEMA` changes from Technical Details
- [x] fixture: `Row` gains `flags` and the insert lists it; a labels insert; the orange-flagged labelled Gmail message and the stale-colour message
- [x] `FlagColor` in `src/mail/mod.rs`; `MessageSummary` gains `flag: Option<FlagColor>` and `labels: Vec<String>`
- [x] SQL from Technical Details: colour column, the label-aware `mailbox` filter with `:visible` in `MESSAGES_SQL` only, `:flag` filter, `LABELS_SQL`; `MessageQuery` gains `flag: Option<FlagColor>` and its test `Default`
- [x] write tests: orange and blue colour read, `null` for `flagged = 0` with colour bits set, `labels` for the Gmail message and `[]` elsewhere, `mailbox=INBOX` finds the labelled message and lists both its labels (the same `labels` as without a filter and by id), an excluded label is neither listed nor matched, the labelled message appears once without `mailbox`
- [x] write tests: with `[Gmail]/All Mail` excluded, the labelled message is absent from the list (also with `mailbox=INBOX`, and a full page is not cut short by rows dropped after `LIMIT`) and its `GET` by id is `404`
- [x] write tests: `flag` filter, cursor paging under the filter, a schema without `labels` or `flags` fails the schema check
- [x] run `mise run check` - must pass before task 2

### Task 2: `flag` query parameter

**Files:**
- Modify: `src/request.rs`
- Modify: `src/server.rs`

- [x] `flag` in the `mail_messages_query` allow-list, parsed into `FlagColor`, `400` text from Technical Details
- [x] write tests in `src/request.rs`: each colour, unknown colour, repeated, empty
- [x] write `oneshot` tests in `src/server.rs`: the filter, and `flag` / `labels` in list and single-message JSON
- [x] run `mise run check` - must pass before task 3

### Task 3: `no_move_mailboxes`

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs` (`describe_mail` destructures `MailConfig` without `..`)
- Modify: `src/mail/fixture.rs` (`Fixture::config()` builds `MailConfig` as a literal; the new list is empty there)

- [x] `RawMail` field, default empty; `MailConfig` list and `is_no_move(path)`, case-insensitive, whole path
- [x] `describe_mail` (`--check-config` and the startup listing) prints `no_move_mailboxes` next to `exclude_mailboxes`
- [x] write tests: absent key is empty, listed paths match in any case, a partial path does not match, a wrong type is a config error, `describe_mail` output carries the list
- [x] run `mise run check` - must pass before task 4

### Task 4: Flag and move scripts in the runner

**Files:**
- Modify: `src/mail/script.rs`

- [x] flag script and `Runner::flag(FlagWrite)` per Technical Details
- [x] move script and `Runner::move_message(MailMove)` per Technical Details, output through `parse_moved`
- [x] both take the existing Mail write lock and deadline, and map errors through the existing classifier
- [x] the `///` doc comments on `Runner` and `ScriptError` describe every Mail write, not junk only
- [x] write tests: exact argv for both (values after the script, a path with spaces, brackets and quotes verbatim, `off`), each error code, empty and garbage stdout, deadline kill through the fake
- [x] write tests: a flag, a move and a junk call on clones of one `Runner` never overlap
- [x] run `mise run check` - must pass before task 5

### Task 5: Move resolution in the store

**Files:**
- Modify: `src/mail/store.rs`

- [x] the resolution function next to `place()`, steps 1-6 of Technical Details, returning `Move`, `AlreadyThere`, `Closed`, `NotInFrom` or `NoTarget`
- [x] write tests: Gmail `INBOX` -> `[Gmail]/All Mail` for the labelled message, `[Gmail]/All Mail` -> `INBOX` for an unlabelled one, Exchange `Inbox` -> another mailbox, case-insensitive input returning stored case
- [x] write tests: `AlreadyThere` for `[Gmail]/All Mail` -> `INBOX` on a message labelled `INBOX`, and for `from` equal to `to`; `Move` (never `AlreadyThere`) for `INBOX` -> `[Gmail]/All Mail` on the labelled message, whose `to` is its source, and for `INBOX` -> `Receipts` when it already carries `Receipts`
- [x] write tests: `from` the message is not in, `to` that does not exist, `to` and `from` in `no_move_mailboxes`, `to` excluded, closed checked before membership, invisible message
- [x] run `mise run check` - must pass before task 6

### Task 6: Flag and move routes

**Files:**
- Modify: `src/request.rs`
- Modify: `src/server.rs`
- Modify: `src/auth/mod.rs`

- [x] `mail_flag_body` (`color`) and `mail_move_body` (`from`, `to`) with `deny_unknown_fields` and the existing `400` texts
- [x] routes `PUT` / `DELETE` `/v1/mail/messages/{id}/flag` and `POST /v1/mail/messages/{id}/move`, `require_json` on `PUT` and `POST`; handlers per Technical Details, `AlreadyThere` answered without the runner, status mapping shared with junk; the `///` doc comment on `Runners.mail` describes every Mail write
- [x] `SCOPE_TABLE` rows (`mail.flag` twice, `mail.move`), `ALL_SCOPES`, `scoped_cases()` and the auth table tests
- [x] request-line log extension with the account name and the colour or `off`
- [x] write `oneshot` tests: set and clear a flag, invisible message, bad colour, bad body, wrong content type, mail off
- [x] write `oneshot` tests: Gmail inbox to All Mail (an `osascript` call with `from` `INBOX`) and back, All Mail into a label the message already carries and `from == to`, both `200` with the same id and no `osascript` call, each `403` and `409`, copy not found gives `id: null`, each runner error, scope refusals for the new scopes
- [x] extend `mail_request_log_carries_rows_but_no_content` to the three routes; `every_route_but_health_has_a_scope` passes
- [x] run `mise run check` - must pass before task 7

### Task 7: Verify acceptance criteria

- [ ] every field, parameter, route and status in Technical Details is implemented and covered by a test
- [ ] `mise run check` and `shellcheck scripts/*.sh` pass
- [ ] the Code-Quality Rules gate holds for the whole diff

### Task 8: [Final] Update documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `Info.plist.template`
- Modify: `.github/workflows/release.yml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] README: intro and security model, `[mail]` example with `no_move_mailboxes` and the note that a trash mailbox left visible by `exclude_mailboxes` must be listed there, or moves into it are allowed, scope table rows, types (`flag`, `labels`), `GET /v1/mail/messages` (`flag`, label-aware `mailbox`, the corrected Gmail note: one row per message with its labels listed, and excluding `[Gmail]/All Mail` hides every Gmail message because labels never grant visibility), the three routes with bodies, answers and status codes, "Not available", logs
- [ ] CLAUDE.md rules and `Info.plist.template` text from Technical Details; the cask caveats in `release.yml` name flag and move next to junk
- [ ] version 0.9.0 in `Cargo.toml` and `Cargo.lock`
- [ ] run `mise run check`
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

**Manual verification on the Mac** (after `brew upgrade` to 0.9.0):
- `PUT .../flag {"color":"orange"}`, then `DELETE .../flag` on a test message; Mail shows, then clears, the orange flag.
- A Gmail inbox message: `GET /v1/mail/messages?mailbox=INBOX` lists it with `INBOX` in `labels`; `POST .../move {"from":"INBOX","to":"[Gmail]/All Mail"}` removes it from the inbox, and the reverse brings it back.
- An Exchange message: `Inbox` -> `Archive` and back.
- A Gmail move between two user labels, which planning did not verify.
- With the junk mailboxes in `no_move_mailboxes`, a move into them answers `403`.

**External updates:**
- Authelia (home-environment): the `hub-mail` client gains `bridge:mail.flag` and `bridge:mail.move`.
- The user's `config.toml` gains `no_move_mailboxes` with the junk mailboxes, since its `exclude_mailboxes` leaves them visible.
- turtle-hub `services/mail`: flag and move tools and `flag` / `labels` in `search_mail`, in its own plan written against this contract.
