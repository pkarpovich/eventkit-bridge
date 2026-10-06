use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, FixedOffset, Local, Utc};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, named_params};
use serde::{Serialize, Serializer};

use crate::config::{
    AccountId, AccountName, MAIL_INDEX_RELATIVE_PATH, MailAccount, MailConfig, MailRootError,
    is_decimal,
};
use crate::mail::emlx::{self, Attachment, Completeness, EmlxError, EmlxFile, MailRoot};
use crate::mail::{MailboxPath, MailboxPathError, MessageId};
use crate::reminders_model::LocalTime;

/// How long a query waits on a lock Mail holds before it fails.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
/// The page size when the client gives none.
pub const DEFAULT_LIMIT: u32 = 25;
/// The largest page a client may ask for.
pub const MAX_LIMIT: u32 = 100;
/// The junk mailbox paths, in order of preference, each matched against a whole path ignoring case.
pub const JUNK_MAILBOXES: [&str; 5] = ["[Gmail]/Spam", "Junk Email", "Junk E-mail", "Junk", "Spam"];
/// The inbox path, matched against a whole path ignoring case.
pub const INBOX_MAILBOX: &str = "INBOX";

const RECIPIENT_TO: i64 = 0;
const RECIPIENT_CC: i64 = 1;

const MAILBOXES_SQL: &str =
    "SELECT ROWID, url, total_count, unread_count FROM mailboxes ORDER BY ROWID";

const MESSAGES_SQL: &str = "
SELECT m.ROWID, m.mailbox, m.date_received, s.subject, su.summary, a.address, a.comment, m.read, m.flagged
FROM messages m
LEFT JOIN subjects s ON s.ROWID = m.subject
LEFT JOIN summaries su ON su.ROWID = m.summary
LEFT JOIN addresses a ON a.ROWID = m.sender
WHERE m.deleted IS NOT 1
  AND m.mailbox IN rarray(:mailboxes)
  AND (:since IS NULL OR m.date_received >= :since)
  AND (:until IS NULL OR m.date_received < :until)
  AND (:unread = 0 OR coalesce(m.read, 0) = 0)
  AND (:cursor_date IS NULL
       OR coalesce(m.date_received, 0) < :cursor_date
       OR (coalesce(m.date_received, 0) = :cursor_date AND m.ROWID < :cursor_id))
  AND (:q IS NULL
       OR instr(ufold(s.subject), :q) > 0
       OR instr(ufold(a.comment), :q) > 0
       OR instr(ufold(a.address), :q) > 0
       OR instr(ufold(su.summary), :q) > 0
       OR EXISTS (SELECT 1 FROM recipients r JOIN addresses ra ON ra.ROWID = r.address
                  WHERE r.message = m.ROWID AND r.type IN (0, 1)
                    AND instr(ufold(ra.address), :q) > 0))
ORDER BY coalesce(m.date_received, 0) DESC, m.ROWID DESC
LIMIT :limit";

const MESSAGE_SQL: &str = "
SELECT m.ROWID, m.mailbox, m.date_received, s.subject, su.summary, a.address, a.comment, m.read, m.flagged
FROM messages m
LEFT JOIN subjects s ON s.ROWID = m.subject
LEFT JOIN summaries su ON su.ROWID = m.summary
LEFT JOIN addresses a ON a.ROWID = m.sender
WHERE m.ROWID = :id
  AND m.deleted IS NOT 1
  AND m.mailbox IN rarray(:mailboxes)";

const MESSAGE_MAILBOX_SQL: &str = "
SELECT mailbox FROM messages
WHERE ROWID = :id
  AND deleted IS NOT 1
  AND mailbox IN rarray(:mailboxes)";

const RECIPIENTS_SQL: &str = "
SELECT r.message, r.type, a.address, a.comment
FROM recipients r
JOIN addresses a ON a.ROWID = r.address
WHERE r.message IN rarray(:messages) AND r.type IN (0, 1)
ORDER BY r.message, r.type, r.position";

const PROBE_SQL: &str = "SELECT count(*) FROM sqlite_master";

const COLUMN_SQL: &str = "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2";

const NEWEST_SQL: &str = "
SELECT max(date_received) FROM messages
WHERE deleted IS NOT 1 AND mailbox IN rarray(:mailboxes)";

const MAILBOX_COUNTS_SQL: &str = "
SELECT mailbox, count(*), max(date_received) FROM messages
WHERE deleted IS NOT 1
GROUP BY mailbox";

const REGISTERED_ACCOUNTS_SQL: &str = "
SELECT a.ZIDENTIFIER, t.ZIDENTIFIER, a.ZACCOUNTDESCRIPTION
FROM ZACCOUNT a
LEFT JOIN ZACCOUNTTYPE t ON t.Z_PK = a.ZACCOUNTTYPE";

const ACCOUNTS_DATABASE: &str = "Accounts/Accounts4.sqlite";

const SCHEMA: [(&str, &[&str]); 6] = [
    (
        "messages",
        &[
            "sender",
            "subject",
            "summary",
            "date_sent",
            "date_received",
            "mailbox",
            "read",
            "flagged",
            "deleted",
            "size",
            "conversation_id",
        ],
    ),
    ("subjects", &["subject"]),
    ("addresses", &["address", "comment"]),
    ("summaries", &["summary"]),
    ("recipients", &["message", "address", "type", "position"]),
    ("mailboxes", &["url", "total_count", "unread_count"]),
];

/// The kind of account a mailbox belongs to, from its URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    /// `ews://`, an Exchange account.
    Exchange,
    /// `imap://`, an IMAP account such as iCloud or Gmail.
    Imap,
    /// `local://`, a mailbox kept only on the Mac.
    Local,
}

impl fmt::Display for AccountKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            AccountKind::Exchange => "exchange",
            AccountKind::Imap => "imap",
            AccountKind::Local => "local",
        };
        f.write_str(name)
    }
}

/// A row of the `mailboxes` table, decoded: `<scheme>://<account uuid>/<url-encoded path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxUrl {
    /// The account kind, from the scheme.
    pub kind: AccountKind,
    /// The account uuid.
    pub account: AccountId,
    /// The decoded mailbox path.
    pub path: MailboxPath,
}

/// Why a mailbox URL could not be decoded; such a mailbox is never shown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MailboxUrlError {
    /// The URL has no `://`.
    #[error("the mailbox URL has no scheme")]
    NoScheme,
    /// The scheme is not `ews`, `imap` or `local`.
    #[error("unknown mailbox URL scheme {0:?}")]
    UnknownScheme(String),
    /// The authority is not a full UUID.
    #[error("the mailbox URL account is not a full UUID")]
    Account,
    /// Nothing follows the authority.
    #[error("the mailbox URL has no path")]
    NoPath,
    /// The path was refused.
    #[error(transparent)]
    Path(#[from] MailboxPathError),
}

impl MailboxUrl {
    /// Decodes a mailbox URL from the `mailboxes` table.
    pub fn parse(url: &str) -> Result<Self, MailboxUrlError> {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(MailboxUrlError::NoScheme);
        };
        let kind = match scheme {
            "ews" => AccountKind::Exchange,
            "imap" => AccountKind::Imap,
            "local" => AccountKind::Local,
            other => return Err(MailboxUrlError::UnknownScheme(other.to_owned())),
        };
        let Some((account, path)) = rest.split_once('/') else {
            return Err(MailboxUrlError::NoPath);
        };
        let Ok(account) = AccountId::parse(account) else {
            return Err(MailboxUrlError::Account);
        };
        let path = MailboxPath::decode(path)?;
        Ok(Self {
            kind,
            account,
            path,
        })
    }
}

/// Why the store could not answer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The mail root could not be located.
    #[error(transparent)]
    Root(#[from] MailRootError),
    /// The mail root could not be resolved, which is how a missing Full Disk Access grant shows up.
    #[error("cannot open the mail root {}: {source}", path.display())]
    RootAccess {
        /// The configured or discovered root.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The Envelope Index could not be opened.
    #[error("cannot open the Envelope Index: {0}")]
    Open(#[source] rusqlite::Error),
    /// A query against the Envelope Index failed.
    #[error("Envelope Index query failed: {0}")]
    Query(#[from] rusqlite::Error),
    /// A requested account name is not configured.
    #[error("unknown mail account \"{0}\"")]
    UnknownAccount(AccountName),
    /// A requested mailbox is not a visible mailbox of the requested accounts.
    #[error("unknown mailbox {0:?}")]
    UnknownMailbox(String),
    /// A message file could not be found or read.
    #[error(transparent)]
    File(#[from] EmlxError),
}

/// A page position: the `date_received` and `ROWID` of the last message of the previous page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    date_received: i64,
    id: i64,
}

/// A cursor that is not URL-safe base64 of `<date_received>:<ROWID>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("undecodable cursor")]
pub struct CursorError;

impl Cursor {
    /// Decodes a cursor a previous page returned.
    pub fn decode(text: &str) -> Result<Self, CursorError> {
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(text) else {
            return Err(CursorError);
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(CursorError);
        };
        let Some((date_received, id)) = text.split_once(':') else {
            return Err(CursorError);
        };
        if !is_decimal(date_received.strip_prefix('-').unwrap_or(date_received)) {
            return Err(CursorError);
        }
        let Ok(date_received) = date_received.parse::<i64>() else {
            return Err(CursorError);
        };
        let Some(id) = MessageId::parse(id) else {
            return Err(CursorError);
        };
        Ok(Self {
            date_received,
            id: id.get(),
        })
    }

    /// Encodes the cursor as URL-safe base64 without padding.
    pub fn encode(&self) -> String {
        let Self { date_received, id } = self;
        URL_SAFE_NO_PAD.encode(format!("{date_received}:{id}"))
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encode())
    }
}

/// Which messages to list by their read state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadFilter {
    /// Read and unread messages.
    #[default]
    Any,
    /// Unread messages only.
    Unread,
}

/// The filters and page of `GET /v1/mail/messages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageQuery {
    /// Configured account names; empty means every configured account.
    pub accounts: Vec<AccountName>,
    /// A decoded mailbox path within those accounts, compared ignoring case.
    pub mailbox: Option<String>,
    /// The earliest `date_received`, inclusive.
    pub since: Option<DateTime<FixedOffset>>,
    /// The latest `date_received`, exclusive.
    pub until: Option<DateTime<FixedOffset>>,
    /// A case-insensitive substring of the subject, sender, recipient addresses or summary.
    pub q: Option<String>,
    /// Which read states to list.
    pub read: ReadFilter,
    /// The page size, clamped to 1 to [`MAX_LIMIT`].
    pub limit: u32,
    /// Where the previous page ended.
    pub cursor: Option<Cursor>,
}

#[cfg(test)]
impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            mailbox: None,
            since: None,
            until: None,
            q: None,
            read: ReadFilter::Any,
            limit: DEFAULT_LIMIT,
            cursor: None,
        }
    }
}

/// A configured account and its visible mailboxes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    /// The configured name.
    pub name: AccountName,
    /// The account kind.
    #[serde(rename = "type")]
    pub kind: AccountKind,
    /// The visible mailboxes, by path.
    pub mailboxes: Vec<Mailbox>,
}

/// A mailbox with Mail's counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mailbox {
    /// The decoded path.
    pub path: String,
    /// How many messages Mail counts in it.
    pub total: i64,
    /// How many of them are unread.
    pub unread: i64,
}

/// A sender or recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Address {
    /// The display name, when Mail stored one.
    pub name: Option<String>,
    /// The email address.
    pub address: String,
}

/// A message as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageSummary {
    /// The `ROWID`.
    pub id: MessageId,
    /// The configured account name.
    pub account: AccountName,
    /// The decoded mailbox path.
    pub mailbox: String,
    /// When the message was received, in the Mac's zone.
    pub date: LocalTime,
    /// The sender.
    pub from: Option<Address>,
    /// The `To` recipients.
    pub to: Vec<Address>,
    /// The subject.
    pub subject: Option<String>,
    /// Mail's stored summary, often absent.
    pub summary: Option<String>,
    /// Whether the message is read.
    pub read: bool,
    /// Whether the message is flagged.
    pub flagged: bool,
    /// Whether an `.emlx` or `.partial.emlx` file exists.
    pub has_body: bool,
}

/// A page of messages, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessagePage {
    /// The messages on this page.
    pub messages: Vec<MessageSummary>,
    /// Where the next page starts; `None` on the last page.
    pub next_cursor: Option<Cursor>,
}

/// One message with its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// The listed fields.
    #[serde(flatten)]
    pub summary: MessageSummary,
    /// The `Cc` recipients.
    pub cc: Vec<Address>,
    /// The plain-text body; `None` when no file exists.
    pub body: Option<String>,
    /// Whether `body` was cut at [`emlx::BODY_CAP`] characters.
    pub body_truncated: bool,
    /// Whether only `.partial.emlx` exists, so the body may be incomplete.
    pub partial: bool,
    /// The attachments, without their contents.
    pub attachments: Vec<Attachment>,
}

/// Whether a mailbox shows through the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// No `mail.exclude_mailboxes` entry names it.
    Visible,
    /// `mail.exclude_mailboxes` names it.
    Excluded,
}

/// A mailbox a message can be moved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveTarget {
    /// The decoded path, in the case Mail stores it.
    pub path: String,
    /// Whether the bridge shows the mailbox.
    pub visibility: Visibility,
}

/// Where a visible message is, with its account's junk mailbox and inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePlace {
    /// The configured account holding the message.
    pub account: MailAccount,
    /// The decoded path of its mailbox, in the case Mail stores it.
    pub mailbox: String,
    /// The account's junk mailbox: the first of [`JUNK_MAILBOXES`] it has.
    pub junk: Option<MoveTarget>,
    /// The account's inbox: [`INBOX_MAILBOX`] in whatever case it is stored.
    pub inbox: Option<MoveTarget>,
}

/// What `/healthz` reports about the mail store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailStatus {
    /// How many accounts are configured, each with mailboxes.
    pub accounts: usize,
    /// Seconds since the newest visible message was received; `None` when there is none.
    pub newest_message_age_s: Option<u64>,
}

/// Why the mail store cannot serve reads as configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProblem {
    /// The Envelope Index cannot be opened or read, which is how a missing Full Disk Access grant shows up.
    NoAccess,
    /// A table or column the bridge reads is missing.
    SchemaChanged,
    /// A configured account has no mailboxes.
    AccountMissing,
}

/// An account as `~/Library/Accounts/Accounts4.sqlite` registers it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegisteredAccount {
    /// The account type, such as `com.apple.account.IMAP`.
    pub account_type: Option<String>,
    /// The description, which for an IMAP account is often its email address.
    pub description: Option<String>,
}

/// An account found in the `mailboxes` table, as the startup listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountListing {
    /// The account uuid.
    pub id: AccountId,
    /// The account kind, from its mailbox URLs.
    pub kind: AccountKind,
    /// The account in `Accounts4.sqlite`, when it is readable and lists it.
    pub registered: Option<RegisteredAccount>,
    /// How many mailboxes it has, excluded ones included.
    pub mailboxes: usize,
    /// How many messages those mailboxes hold, deleted rows left out.
    pub messages: i64,
    /// When the newest of them was received.
    pub newest: Option<LocalTime>,
    /// The configured name; `None` when the account is invisible.
    pub name: Option<AccountName>,
}

/// Read access to Mail's store under the `[mail]` config.
#[derive(Debug, Clone)]
pub struct MailStore {
    config: MailConfig,
}

/// One read-only connection to the Envelope Index, for one request.
pub struct MailReader<'a> {
    config: &'a MailConfig,
    root: MailRoot,
    conn: Connection,
}

struct MailboxRecord {
    id: i64,
    url: MailboxUrl,
    total: i64,
    unread: i64,
}

struct Tally {
    listing: AccountListing,
    newest: Option<i64>,
}

struct VisibleMailbox {
    id: i64,
    account: MailAccount,
    kind: AccountKind,
    path: MailboxPath,
    display: String,
    total: i64,
    unread: i64,
}

struct SummaryRow {
    id: MessageId,
    mailbox: i64,
    date_received: i64,
    subject: Option<String>,
    summary: Option<String>,
    sender: Option<Address>,
    read: bool,
    flagged: bool,
}

#[derive(Default)]
struct Recipients {
    to: Vec<Address>,
    cc: Vec<Address>,
}

struct SummaryParts<'m> {
    row: SummaryRow,
    mailbox: &'m VisibleMailbox,
    to: Vec<Address>,
    has_body: bool,
}

impl MailStore {
    /// A store reading under `config`; nothing is opened until [`MailStore::open`].
    pub fn new(config: MailConfig) -> Self {
        Self { config }
    }

    /// Locates the root and opens the Envelope Index read-only.
    pub fn open(&self) -> Result<MailReader<'_>, StoreError> {
        let path = self.config.root()?;
        let root = match MailRoot::new(&path) {
            Ok(root) => root,
            Err(source) => return Err(StoreError::RootAccess { path, source }),
        };
        let conn = match open_index(&root.as_path().join(MAIL_INDEX_RELATIVE_PATH)) {
            Ok(conn) => conn,
            Err(err) => return Err(StoreError::Open(err)),
        };
        Ok(MailReader {
            config: &self.config,
            root,
            conn,
        })
    }

    /// Opens the store and checks the schema, the configured accounts and the newest message as of `now`.
    pub fn status(&self, now: DateTime<Utc>) -> Result<MailStatus, MailProblem> {
        let reader = match self.open() {
            Ok(reader) => reader,
            Err(err) => return Err(unreadable(&err)),
        };
        reader.status(now)
    }
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// Opens an Envelope Index read-only, with the busy timeout, `query_only`, `ufold` and `rarray`.
///
/// Never `immutable`: Mail writes the WAL while the bridge reads.
pub fn open_index(path: &Path) -> rusqlite::Result<Connection> {
    let conn = open_read_only(path)?;
    conn.create_scalar_function(
        "ufold",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let value: Option<String> = ctx.get(0)?;
            Ok(value.map(|value| value.to_lowercase()))
        },
    )?;
    rusqlite::vtab::array::load_module(&conn)?;
    Ok(conn)
}

impl MailReader<'_> {
    /// The configured accounts that have visible mailboxes, by name.
    pub fn accounts(&self) -> Result<Vec<Account>, StoreError> {
        let mailboxes = self.visible_mailboxes()?;
        let mut accounts = Vec::new();
        for MailAccount { id, name } in &self.config.accounts {
            let mut kind = None;
            let mut listed = Vec::new();
            for mailbox in &mailboxes {
                if mailbox.account.id != *id {
                    continue;
                }
                kind.get_or_insert(mailbox.kind);
                listed.push(Mailbox {
                    path: mailbox.display.clone(),
                    total: mailbox.total,
                    unread: mailbox.unread,
                });
            }
            let Some(kind) = kind else { continue };
            listed.sort_by(|a, b| a.path.cmp(&b.path));
            accounts.push(Account {
                name: name.clone(),
                kind,
                mailboxes: listed,
            });
        }
        Ok(accounts)
    }

    /// One page of visible messages matching `query`, newest first by `date_received`.
    pub fn messages(&self, query: &MessageQuery) -> Result<MessagePage, StoreError> {
        let MessageQuery {
            accounts: _,
            mailbox: _,
            since,
            until,
            q,
            read,
            limit,
            cursor,
        } = query;
        let mailboxes = self.visible_mailboxes()?;
        let selected = self.select(&mailboxes, query)?;
        let mut ids = Vec::new();
        for mailbox in &selected {
            ids.push(Value::from(mailbox.id));
        }
        let limit = (*limit).clamp(1, MAX_LIMIT);
        let q = match q {
            Some(q) if !q.is_empty() => Some(q.to_lowercase()),
            Some(_) | None => None,
        };
        let unread = match read {
            ReadFilter::Any => false,
            ReadFilter::Unread => true,
        };
        let (cursor_date, cursor_id) = match cursor {
            Some(Cursor { date_received, id }) => (Some(*date_received), Some(*id)),
            None => (None, None),
        };

        let mut stmt = self.conn.prepare_cached(MESSAGES_SQL)?;
        let mut rows = stmt.query(named_params! {
            ":mailboxes": Rc::new(ids),
            ":since": since.map(|at| at.timestamp()),
            ":until": until.map(|at| at.timestamp()),
            ":unread": unread,
            ":cursor_date": cursor_date,
            ":cursor_id": cursor_id,
            ":q": q,
            ":limit": i64::from(limit) + 1,
        })?;
        let mut found = Vec::new();
        while let Some(row) = rows.next()? {
            let Some(row) = summary_row(row)? else {
                continue;
            };
            found.push(row);
        }

        let limit = usize::try_from(limit).unwrap_or(1);
        let mut next_cursor = None;
        if found.len() > limit {
            found.truncate(limit);
            if let Some(last) = found.last() {
                next_cursor = Some(Cursor {
                    date_received: last.date_received,
                    id: last.id.get(),
                });
            }
        }

        let mut message_ids = Vec::new();
        for row in &found {
            message_ids.push(row.id);
        }
        let mut recipients = self.recipients(&message_ids)?;
        let mut messages = Vec::new();
        for row in found {
            let Some(mailbox) = find_mailbox(&mailboxes, row.mailbox) else {
                continue;
            };
            let has_body = match self.locate(mailbox, row.id) {
                Ok(file) => file.is_some(),
                Err(_) => {
                    tracing::warn!(message = %row.id, "message file refused");
                    false
                }
            };
            let Recipients { to, cc: _ } = recipients.remove(&row.id).unwrap_or_default();
            messages.push(
                SummaryParts {
                    row,
                    mailbox,
                    to,
                    has_body,
                }
                .into_summary(),
            );
        }
        Ok(MessagePage {
            messages,
            next_cursor,
        })
    }

    /// The visible message `id` with its body; `None` when it is deleted, excluded, in an unconfigured account or unknown.
    pub fn message(&self, id: MessageId) -> Result<Option<Message>, StoreError> {
        let mailboxes = self.visible_mailboxes()?;
        let mut ids = Vec::new();
        for mailbox in &mailboxes {
            ids.push(Value::from(mailbox.id));
        }
        let mut stmt = self.conn.prepare_cached(MESSAGE_SQL)?;
        let row = stmt
            .query_row(
                named_params! { ":id": id.get(), ":mailboxes": Rc::new(ids) },
                summary_row,
            )
            .optional()?;
        let Some(Some(row)) = row else {
            return Ok(None);
        };
        let Some(mailbox) = find_mailbox(&mailboxes, row.mailbox) else {
            return Ok(None);
        };
        let Recipients { to, cc } = self.recipients(&[id])?.remove(&id).unwrap_or_default();
        let file = self.locate(mailbox, id)?;
        let summary = SummaryParts {
            row,
            mailbox,
            to,
            has_body: file.is_some(),
        }
        .into_summary();
        let Some(file) = file else {
            return Ok(Some(Message {
                summary,
                cc,
                body: None,
                body_truncated: false,
                partial: false,
                attachments: Vec::new(),
            }));
        };
        let emlx::Content {
            body,
            body_truncated,
            attachments,
        } = emlx::read(&file)?;
        let partial = match file.completeness {
            Completeness::Full => false,
            Completeness::Partial => true,
        };
        Ok(Some(Message {
            summary,
            cc,
            body: Some(body),
            body_truncated,
            partial,
            attachments,
        }))
    }

    /// Where the visible message `id` is and where it can be moved; `None` when it is deleted,
    /// excluded, in an unconfigured account or unknown.
    pub fn place(&self, id: MessageId) -> Result<Option<MessagePlace>, StoreError> {
        let mailboxes = self.visible_mailboxes()?;
        let mut ids = Vec::new();
        for mailbox in &mailboxes {
            ids.push(Value::from(mailbox.id));
        }
        let mut stmt = self.conn.prepare_cached(MESSAGE_MAILBOX_SQL)?;
        let mailbox: Option<i64> = stmt
            .query_row(
                named_params! { ":id": id.get(), ":mailboxes": Rc::new(ids) },
                |row| row.get(0),
            )
            .optional()?;
        let Some(mailbox) = mailbox else {
            return Ok(None);
        };
        let Some(mailbox) = find_mailbox(&mailboxes, mailbox) else {
            return Ok(None);
        };
        let mut paths = Vec::new();
        for MailboxRecord {
            id: _,
            url,
            total: _,
            unread: _,
        } in self.mailbox_records()?
        {
            if url.account == mailbox.account.id {
                paths.push(url.path.to_string());
            }
        }
        let mut junk = None;
        for candidate in JUNK_MAILBOXES {
            let Some(target) = self.target(&paths, candidate) else {
                continue;
            };
            junk = Some(target);
            break;
        }
        Ok(Some(MessagePlace {
            account: mailbox.account.clone(),
            mailbox: mailbox.display.clone(),
            junk,
            inbox: self.target(&paths, INBOX_MAILBOX),
        }))
    }

    /// Every account in the `mailboxes` table with its counts, the configured name and, when
    /// `Accounts4.sqlite` is readable, its type and description.
    pub fn listing(&self) -> Result<Vec<AccountListing>, StoreError> {
        let records = self.mailbox_records()?;
        let mut counts = HashMap::new();
        let mut stmt = self.conn.prepare_cached(MAILBOX_COUNTS_SQL)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let mailbox: i64 = row.get(0)?;
            let messages: i64 = row.get(1)?;
            let newest: Option<i64> = row.get(2)?;
            counts.insert(mailbox, (messages, newest));
        }
        let mut registered = self.registered_accounts();
        let mut tallies: BTreeMap<AccountId, Tally> = BTreeMap::new();
        for MailboxRecord {
            id,
            url:
                MailboxUrl {
                    kind,
                    account,
                    path: _,
                },
            total: _,
            unread: _,
        } in records
        {
            let (messages, newest) = counts.get(&id).copied().unwrap_or((0, None));
            let tally = tallies.entry(account.clone()).or_insert_with(|| Tally {
                listing: AccountListing {
                    registered: registered.remove(&account),
                    name: self
                        .config
                        .account(&account)
                        .map(|found| found.name.clone()),
                    id: account,
                    kind,
                    mailboxes: 0,
                    messages: 0,
                    newest: None,
                },
                newest: None,
            });
            tally.listing.mailboxes += 1;
            tally.listing.messages += messages;
            tally.newest = tally.newest.max(newest);
        }
        let mut listings = Vec::new();
        for Tally {
            mut listing,
            newest,
        } in tallies.into_values()
        {
            listing.newest = newest.map(local_time);
            listings.push(listing);
        }
        Ok(listings)
    }

    fn status(&self, now: DateTime<Utc>) -> Result<MailStatus, MailProblem> {
        if let Err(err) = self
            .conn
            .query_row(PROBE_SQL, [], |row| row.get::<_, i64>(0))
        {
            return Err(unreadable(&err));
        }
        match self.missing_column() {
            Ok(None) => {}
            Ok(Some((table, column))) => {
                tracing::warn!(table, column, "the Envelope Index schema changed");
                return Err(MailProblem::SchemaChanged);
            }
            Err(err) => return Err(unreadable(&err)),
        }
        let records = match self.mailbox_records() {
            Ok(records) => records,
            Err(err) => return Err(unreadable(&err)),
        };
        for MailAccount { id, name } in &self.config.accounts {
            let mut found = false;
            for record in &records {
                if record.url.account == *id {
                    found = true;
                    break;
                }
            }
            if !found {
                tracing::warn!(account = %name, "a configured mail account has no mailboxes");
                return Err(MailProblem::AccountMissing);
            }
        }
        let newest = match self.newest() {
            Ok(newest) => newest,
            Err(err) => return Err(unreadable(&err)),
        };
        let newest_message_age_s =
            newest.map(|newest| u64::try_from(now.timestamp() - newest).unwrap_or(0));
        Ok(MailStatus {
            accounts: self.config.accounts.len(),
            newest_message_age_s,
        })
    }

    fn missing_column(&self) -> rusqlite::Result<Option<(&'static str, &'static str)>> {
        let mut stmt = self.conn.prepare_cached(COLUMN_SQL)?;
        for (table, columns) in SCHEMA {
            for column in columns {
                let found: i64 = stmt.query_row([table, column], |row| row.get(0))?;
                if found == 0 {
                    return Ok(Some((table, column)));
                }
            }
        }
        Ok(None)
    }

    fn newest(&self) -> rusqlite::Result<Option<i64>> {
        let mut ids = Vec::new();
        for mailbox in self.visible_mailboxes()? {
            ids.push(Value::from(mailbox.id));
        }
        let mut stmt = self.conn.prepare_cached(NEWEST_SQL)?;
        stmt.query_row(named_params! { ":mailboxes": Rc::new(ids) }, |row| {
            row.get(0)
        })
    }

    fn registered_accounts(&self) -> HashMap<AccountId, RegisteredAccount> {
        let Some(library) = self.root.as_path().parent().and_then(Path::parent) else {
            return HashMap::new();
        };
        match registered_accounts(&library.join(ACCOUNTS_DATABASE)) {
            Ok(registered) => registered,
            Err(err) => {
                tracing::warn!(error = %err, "cannot read Accounts4.sqlite, account descriptions left out");
                HashMap::new()
            }
        }
    }

    fn mailbox_records(&self) -> rusqlite::Result<Vec<MailboxRecord>> {
        let mut stmt = self.conn.prepare_cached(MAILBOXES_SQL)?;
        let mut rows = stmt.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let url: Option<String> = row.get(1)?;
            let total: Option<i64> = row.get(2)?;
            let unread: Option<i64> = row.get(3)?;
            let Some(url) = url else { continue };
            let url = match MailboxUrl::parse(&url) {
                Ok(url) => url,
                Err(err) => {
                    tracing::debug!(mailbox = id, error = %err, "mailbox URL skipped");
                    continue;
                }
            };
            records.push(MailboxRecord {
                id,
                url,
                total: total.unwrap_or(0),
                unread: unread.unwrap_or(0),
            });
        }
        Ok(records)
    }

    fn visible_mailboxes(&self) -> rusqlite::Result<Vec<VisibleMailbox>> {
        let mut visible = Vec::new();
        for MailboxRecord {
            id,
            url:
                MailboxUrl {
                    kind,
                    account,
                    path,
                },
            total,
            unread,
        } in self.mailbox_records()?
        {
            let Some(account) = self.config.account(&account) else {
                continue;
            };
            let display = path.to_string();
            if self.config.is_excluded(&display) {
                continue;
            }
            visible.push(VisibleMailbox {
                id,
                account: account.clone(),
                kind,
                path,
                display,
                total,
                unread,
            });
        }
        Ok(visible)
    }

    fn select<'m>(
        &self,
        mailboxes: &'m [VisibleMailbox],
        query: &MessageQuery,
    ) -> Result<Vec<&'m VisibleMailbox>, StoreError> {
        let mut accounts = Vec::new();
        for name in &query.accounts {
            let mut found = None;
            for account in &self.config.accounts {
                if account.name == *name {
                    found = Some(&account.id);
                    break;
                }
            }
            let Some(id) = found else {
                return Err(StoreError::UnknownAccount(name.clone()));
            };
            accounts.push(id);
        }
        let wanted = query.mailbox.as_ref().map(|path| path.to_lowercase());
        let mut selected = Vec::new();
        for mailbox in mailboxes {
            if !accounts.is_empty() && !accounts.contains(&&mailbox.account.id) {
                continue;
            }
            if let Some(wanted) = &wanted
                && mailbox.display.to_lowercase() != *wanted
            {
                continue;
            }
            selected.push(mailbox);
        }
        if let Some(path) = &query.mailbox
            && selected.is_empty()
        {
            return Err(StoreError::UnknownMailbox(path.clone()));
        }
        Ok(selected)
    }

    fn target(&self, paths: &[String], wanted: &str) -> Option<MoveTarget> {
        let wanted = wanted.to_lowercase();
        for path in paths {
            if path.to_lowercase() != wanted {
                continue;
            }
            let visibility = match self.config.is_excluded(path) {
                true => Visibility::Excluded,
                false => Visibility::Visible,
            };
            return Some(MoveTarget {
                path: path.clone(),
                visibility,
            });
        }
        None
    }

    fn recipients(
        &self,
        messages: &[MessageId],
    ) -> rusqlite::Result<HashMap<MessageId, Recipients>> {
        let mut ids = Vec::new();
        for id in messages {
            ids.push(Value::from(id.get()));
        }
        let mut stmt = self.conn.prepare_cached(RECIPIENTS_SQL)?;
        let mut rows = stmt.query(named_params! { ":messages": Rc::new(ids) })?;
        let mut recipients: HashMap<MessageId, Recipients> = HashMap::new();
        while let Some(row) = rows.next()? {
            let message: i64 = row.get(0)?;
            let kind: Option<i64> = row.get(1)?;
            let Some(message) = MessageId::new(message) else {
                continue;
            };
            let Some(address) = address(row.get(2)?, row.get(3)?) else {
                continue;
            };
            let entry = recipients.entry(message).or_default();
            match kind {
                Some(RECIPIENT_TO) => entry.to.push(address),
                Some(RECIPIENT_CC) => entry.cc.push(address),
                Some(_) | None => {}
            }
        }
        Ok(recipients)
    }

    fn locate(
        &self,
        mailbox: &VisibleMailbox,
        id: MessageId,
    ) -> Result<Option<EmlxFile>, EmlxError> {
        let dir = self.root.mailbox_dir(&mailbox.account.id, &mailbox.path);
        emlx::find(&self.root, &dir, id)
    }
}

impl SummaryParts<'_> {
    fn into_summary(self) -> MessageSummary {
        let SummaryParts {
            row:
                SummaryRow {
                    id,
                    mailbox: _,
                    date_received,
                    subject,
                    summary,
                    sender,
                    read,
                    flagged,
                },
            mailbox,
            to,
            has_body,
        } = self;
        MessageSummary {
            id,
            account: mailbox.account.name.clone(),
            mailbox: mailbox.display.clone(),
            date: local_time(date_received),
            from: sender,
            to,
            subject,
            summary,
            read,
            flagged,
            has_body,
        }
    }
}

fn unreadable(err: &dyn fmt::Display) -> MailProblem {
    tracing::warn!(error = %err, "cannot read the mail store");
    MailProblem::NoAccess
}

fn registered_accounts(path: &Path) -> rusqlite::Result<HashMap<AccountId, RegisteredAccount>> {
    let conn = open_read_only(path)?;
    let mut stmt = conn.prepare(REGISTERED_ACCOUNTS_SQL)?;
    let mut rows = stmt.query([])?;
    let mut registered = HashMap::new();
    while let Some(row) = rows.next()? {
        let id: Option<String> = row.get(0)?;
        let Some(id) = id else { continue };
        let Ok(id) = AccountId::parse(&id) else {
            continue;
        };
        registered.insert(
            id,
            RegisteredAccount {
                account_type: row.get(1)?,
                description: row.get(2)?,
            },
        );
    }
    Ok(registered)
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<SummaryRow>> {
    let id: i64 = row.get(0)?;
    let Some(id) = MessageId::new(id) else {
        return Ok(None);
    };
    let read: Option<i64> = row.get(7)?;
    let flagged: Option<i64> = row.get(8)?;
    let date_received: Option<i64> = row.get(2)?;
    Ok(Some(SummaryRow {
        id,
        mailbox: row.get(1)?,
        date_received: date_received.unwrap_or(0),
        subject: row.get(3)?,
        summary: row.get(4)?,
        sender: address(row.get(5)?, row.get(6)?),
        read: read.unwrap_or(0) != 0,
        flagged: flagged.unwrap_or(0) != 0,
    }))
}

fn address(address: Option<String>, comment: Option<String>) -> Option<Address> {
    let address = address?;
    let name = match comment {
        Some(name) if !name.is_empty() => Some(name),
        Some(_) | None => None,
    };
    Some(Address { name, address })
}

fn find_mailbox(mailboxes: &[VisibleMailbox], id: i64) -> Option<&VisibleMailbox> {
    mailboxes.iter().find(|mailbox| mailbox.id == id)
}

fn local_time(seconds: i64) -> LocalTime {
    let utc = DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_default();
    LocalTime::from(utc.with_timezone(&Local).fixed_offset())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use super::*;
    use crate::mail::fixture::{self, Fixture, MailboxRow, Row};

    fn name(value: &str) -> AccountName {
        AccountName::parse(value).unwrap()
    }

    fn at(seconds: i64) -> DateTime<FixedOffset> {
        DateTime::<Utc>::from_timestamp(seconds, 0)
            .unwrap()
            .fixed_offset()
    }

    fn message_id(value: i64) -> MessageId {
        MessageId::new(value).unwrap()
    }

    fn ids(page: &MessagePage) -> Vec<i64> {
        let mut ids = Vec::new();
        for message in &page.messages {
            ids.push(message.id.get());
        }
        ids
    }

    fn list(fixture: &Fixture, query: MessageQuery) -> Result<Vec<i64>, StoreError> {
        let store = MailStore::new(fixture.config());
        let reader = store.open()?;
        let page = reader.messages(&query)?;
        Ok(ids(&page))
    }

    fn get(fixture: &Fixture, id: i64) -> Option<Message> {
        let store = MailStore::new(fixture.config());
        store.open().unwrap().message(message_id(id)).unwrap()
    }

    const ALL_VISIBLE: [i64; 6] = [
        fixture::MULTIPART_ALL_MAIL,
        fixture::MULTIPART,
        fixture::HTML,
        fixture::PLAIN,
        fixture::PARTIAL,
        fixture::MISSING,
    ];

    #[test]
    fn mailbox_url_decodes_each_scheme() {
        let url = MailboxUrl::parse(&format!("ews://{}/Inbox", fixture::MAIN)).unwrap();
        assert_eq!(url.kind, AccountKind::Exchange);
        assert_eq!(url.account, AccountId::parse(fixture::MAIN).unwrap());
        assert_eq!(url.path.to_string(), "Inbox");

        let url = MailboxUrl::parse(&format!(
            "imap://{}/%5BGmail%5D/All%20Mail",
            fixture::GMAIL.to_ascii_lowercase()
        ))
        .unwrap();
        assert_eq!(url.kind, AccountKind::Imap);
        assert_eq!(url.account, AccountId::parse(fixture::GMAIL).unwrap());
        assert_eq!(url.path.segments(), ["[Gmail]", "All Mail"]);

        let url = MailboxUrl::parse(&format!("local://{}/Archive", fixture::OTHER)).unwrap();
        assert_eq!(url.kind, AccountKind::Local);
    }

    #[test]
    fn mailbox_url_refuses_malformed_urls() {
        let cases = [
            ("Inbox".to_owned(), MailboxUrlError::NoScheme),
            (
                format!("pop://{}/Inbox", fixture::MAIN),
                MailboxUrlError::UnknownScheme("pop".to_owned()),
            ),
            ("imap://account/Inbox".to_owned(), MailboxUrlError::Account),
            (format!("imap://{}", fixture::MAIN), MailboxUrlError::NoPath),
            (
                format!("imap://{}/", fixture::MAIN),
                MailboxUrlError::Path(MailboxPathError::Empty),
            ),
            (
                format!("imap://{}/INBOX/..%2F..%2Fetc", fixture::MAIN),
                MailboxUrlError::Path(MailboxPathError::Separator),
            ),
            (
                format!("imap://{}/../../etc", fixture::MAIN),
                MailboxUrlError::Path(MailboxPathError::DotSegment),
            ),
        ];
        for (url, error) in cases {
            assert_eq!(MailboxUrl::parse(&url), Err(error), "{url}");
        }
    }

    #[test]
    fn accounts_lists_configured_accounts_without_excluded_mailboxes() {
        let fixture = Fixture::standard();
        let store = MailStore::new(fixture.config());
        let accounts = store.open().unwrap().accounts().unwrap();
        assert_eq!(
            accounts,
            vec![
                Account {
                    name: name("gmail"),
                    kind: AccountKind::Imap,
                    mailboxes: vec![
                        Mailbox {
                            path: "INBOX".to_owned(),
                            total: 1,
                            unread: 1,
                        },
                        Mailbox {
                            path: "[Gmail]/All Mail".to_owned(),
                            total: 1,
                            unread: 1,
                        },
                    ],
                },
                Account {
                    name: name("main"),
                    kind: AccountKind::Exchange,
                    mailboxes: vec![Mailbox {
                        path: "Inbox".to_owned(),
                        total: 4,
                        unread: 2,
                    }],
                },
            ]
        );
    }

    #[test]
    fn accounts_skips_a_configured_account_without_mailboxes() {
        let fixture = Fixture::standard();
        let mut config = fixture.config();
        config.accounts.push(MailAccount {
            id: AccountId::parse("11111111-2222-3333-4444-555555555555").unwrap(),
            name: name("gone"),
        });
        config.exclude_mailboxes.push("inbox".to_owned());
        let store = MailStore::new(config);
        let accounts = store.open().unwrap().accounts().unwrap();
        let mut names = Vec::new();
        for account in &accounts {
            names.push(account.name.as_str());
        }
        assert_eq!(names, ["gmail"]);
        assert_eq!(accounts[0].mailboxes.len(), 1);
        assert_eq!(accounts[0].mailboxes[0].path, "[Gmail]/All Mail");
    }

    #[test]
    fn messages_lists_every_visible_row_newest_first() {
        let fixture = Fixture::standard();
        assert_eq!(
            list(&fixture, MessageQuery::default()).unwrap(),
            ALL_VISIBLE
        );
    }

    #[test]
    fn messages_never_lists_excluded_deleted_unconfigured_or_crafted_rows() {
        let fixture = Fixture::standard();
        let listed = list(
            &fixture,
            MessageQuery {
                limit: MAX_LIMIT,
                ..MessageQuery::default()
            },
        )
        .unwrap();
        for hidden in [
            fixture::IN_DELETED_ITEMS,
            fixture::DELETED_ROW,
            fixture::UNCONFIGURED,
            fixture::IN_SPAM,
            fixture::IN_CRAFTED,
        ] {
            assert!(!listed.contains(&hidden), "{hidden}");
        }
    }

    #[test]
    fn messages_summarise_every_field() {
        let fixture = Fixture::standard();
        let store = MailStore::new(fixture.config());
        let page = store
            .open()
            .unwrap()
            .messages(&MessageQuery {
                q: Some("quarterly".to_owned()),
                ..MessageQuery::default()
            })
            .unwrap();
        let date = DateTime::<Utc>::from_timestamp(fixture::T + 100, 0)
            .unwrap()
            .with_timezone(&Local)
            .fixed_offset();
        assert_eq!(
            page,
            MessagePage {
                messages: vec![MessageSummary {
                    id: message_id(fixture::PLAIN),
                    account: name("main"),
                    mailbox: "Inbox".to_owned(),
                    date: LocalTime::from(date),
                    from: Some(Address {
                        name: Some("Alice Example".to_owned()),
                        address: "alice@example.com".to_owned(),
                    }),
                    to: vec![Address {
                        name: Some("Bob".to_owned()),
                        address: "bob@example.com".to_owned(),
                    }],
                    subject: Some("Quarterly report".to_owned()),
                    summary: Some("Numbers attached".to_owned()),
                    read: true,
                    flagged: false,
                    has_body: true,
                }],
                next_cursor: None,
            }
        );
    }

    #[test]
    fn messages_report_flags_and_files() {
        let fixture = Fixture::standard();
        let store = MailStore::new(fixture.config());
        let page = store
            .open()
            .unwrap()
            .messages(&MessageQuery::default())
            .unwrap();
        let mut seen = Vec::new();
        for message in &page.messages {
            seen.push((
                message.id.get(),
                message.read,
                message.flagged,
                message.has_body,
            ));
        }
        assert_eq!(
            seen,
            [
                (fixture::MULTIPART_ALL_MAIL, false, false, true),
                (fixture::MULTIPART, false, false, true),
                (fixture::HTML, false, true, true),
                (fixture::PLAIN, true, false, true),
                (fixture::PARTIAL, true, false, true),
                (fixture::MISSING, false, false, false),
            ]
        );
        let first = &page.messages[0];
        assert_eq!(first.account, name("gmail"));
        assert_eq!(first.mailbox, "[Gmail]/All Mail");
        assert_eq!(first.to[0].name, None);
        assert_eq!(page.messages[1].mailbox, "INBOX");
        assert_eq!(
            page.messages[1].subject.as_deref(),
            Some(fixture::CYRILLIC_SUBJECT)
        );
    }

    #[test]
    fn messages_filter_by_account() {
        let fixture = Fixture::standard();
        let by = |accounts: &[&str]| {
            let mut names = Vec::new();
            for account in accounts {
                names.push(name(account));
            }
            list(
                &fixture,
                MessageQuery {
                    accounts: names,
                    ..MessageQuery::default()
                },
            )
        };
        assert_eq!(
            by(&["gmail"]).unwrap(),
            [fixture::MULTIPART_ALL_MAIL, fixture::MULTIPART]
        );
        assert_eq!(
            by(&["main"]).unwrap(),
            [
                fixture::HTML,
                fixture::PLAIN,
                fixture::PARTIAL,
                fixture::MISSING
            ]
        );
        assert_eq!(by(&["main", "gmail"]).unwrap(), ALL_VISIBLE);
        let result = by(&["main", "nope"]);
        let Err(StoreError::UnknownAccount(account)) = &result else {
            panic!("{result:?}");
        };
        assert_eq!(account.as_str(), "nope");
    }

    #[test]
    fn messages_filter_by_mailbox_ignoring_case() {
        let fixture = Fixture::standard();
        let by = |accounts: &[&str], mailbox: &str| {
            let mut names = Vec::new();
            for account in accounts {
                names.push(name(account));
            }
            list(
                &fixture,
                MessageQuery {
                    accounts: names,
                    mailbox: Some(mailbox.to_owned()),
                    ..MessageQuery::default()
                },
            )
        };
        assert_eq!(
            by(&[], "inbox").unwrap(),
            [
                fixture::MULTIPART,
                fixture::HTML,
                fixture::PLAIN,
                fixture::PARTIAL,
                fixture::MISSING
            ]
        );
        assert_eq!(by(&["gmail"], "INBOX").unwrap(), [fixture::MULTIPART]);
        assert_eq!(
            by(&[], "[gmail]/all mail").unwrap(),
            [fixture::MULTIPART_ALL_MAIL]
        );
        for (accounts, mailbox) in [
            (&["main"][..], "[Gmail]/All Mail"),
            (&[][..], "Deleted Items"),
            (&[][..], "[Gmail]/Spam"),
            (&[][..], "INBOX/../../etc"),
            (&[][..], "Nope"),
        ] {
            let result = by(accounts, mailbox);
            let Err(StoreError::UnknownMailbox(path)) = &result else {
                panic!("{mailbox}: {result:?}");
            };
            assert_eq!(path, mailbox);
        }
    }

    #[test]
    fn messages_filter_by_received_date() {
        let fixture = Fixture::standard();
        let since = list(
            &fixture,
            MessageQuery {
                since: Some(at(fixture::T + 200)),
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(
            since,
            [
                fixture::MULTIPART_ALL_MAIL,
                fixture::MULTIPART,
                fixture::HTML
            ]
        );
        let until = list(
            &fixture,
            MessageQuery {
                until: Some(at(fixture::T + 200)),
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(until, [fixture::PLAIN, fixture::PARTIAL, fixture::MISSING]);
        let between = list(
            &fixture,
            MessageQuery {
                since: Some(at(fixture::T + 50)),
                until: Some(at(fixture::T + 200)),
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(between, [fixture::PLAIN, fixture::PARTIAL]);
    }

    #[test]
    fn messages_match_q_case_insensitively_across_fields() {
        let fixture = Fixture::standard();
        let by = |q: &str| {
            list(
                &fixture,
                MessageQuery {
                    q: Some(q.to_owned()),
                    ..MessageQuery::default()
                },
            )
            .unwrap()
        };
        let cyrillic = [fixture::MULTIPART_ALL_MAIL, fixture::MULTIPART];
        assert_eq!(by("QUARTERLY"), [fixture::PLAIN]);
        assert_eq!(by("привет из"), cyrillic);
        assert_eq!(by("МИНСКА"), cyrillic);
        assert_eq!(by("иван"), cyrillic);
        assert_eq!(by("IVAN@EXAMPLE"), cyrillic);
        assert_eq!(by("alice example"), [fixture::PLAIN]);
        assert_eq!(by("carol@"), [fixture::PLAIN]);
        assert_eq!(by("bob@example"), [fixture::HTML, fixture::PLAIN]);
        assert_eq!(by("numbers ATTACHED"), [fixture::PLAIN]);
        assert_eq!(by("пятницу"), cyrillic);
        assert_eq!(by("nothing like this"), Vec::<i64>::new());
        assert_eq!(by(""), ALL_VISIBLE);
    }

    #[test]
    fn messages_q_ignores_other_recipient_types() {
        let fixture = Fixture::standard();
        let conn = fixture.writer();
        conn.execute(
            "INSERT INTO addresses (address, comment) VALUES ('hidden@example.com', 'Hidden')",
            [],
        )
        .unwrap();
        let address = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO recipients (message, address, type, position) VALUES (?1, ?2, 2, 0)",
            rusqlite::params![fixture::PLAIN, address],
        )
        .unwrap();
        let found = list(
            &fixture,
            MessageQuery {
                q: Some("hidden@".to_owned()),
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn messages_filter_unread() {
        let fixture = Fixture::standard();
        let unread = list(
            &fixture,
            MessageQuery {
                read: ReadFilter::Unread,
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(
            unread,
            [
                fixture::MULTIPART_ALL_MAIL,
                fixture::MULTIPART,
                fixture::HTML,
                fixture::MISSING
            ]
        );
    }

    #[test]
    fn messages_combine_filters() {
        let fixture = Fixture::standard();
        let query = MessageQuery {
            accounts: vec![name("gmail")],
            mailbox: Some("inbox".to_owned()),
            q: Some("ПРИВЕТ".to_owned()),
            read: ReadFilter::Unread,
            since: Some(at(fixture::T)),
            until: Some(at(fixture::T + 301)),
            ..MessageQuery::default()
        };
        assert_eq!(list(&fixture, query.clone()).unwrap(), [fixture::MULTIPART]);
        let later = MessageQuery {
            since: Some(at(fixture::T + 301)),
            ..query.clone()
        };
        assert!(list(&fixture, later).unwrap().is_empty());
        let main = MessageQuery {
            accounts: vec![name("main")],
            ..query
        };
        assert!(list(&fixture, main).unwrap().is_empty());
        let main_unread_with_bob = MessageQuery {
            accounts: vec![name("main")],
            q: Some("bob".to_owned()),
            read: ReadFilter::Unread,
            ..MessageQuery::default()
        };
        assert_eq!(
            list(&fixture, main_unread_with_bob).unwrap(),
            [fixture::HTML]
        );
    }

    #[test]
    fn messages_page_through_equal_timestamps() {
        let fixture = Fixture::empty();
        fixture.mailbox(&MailboxRow {
            id: fixture::MAIN_INBOX,
            url: format!("ews://{}/Inbox", fixture::MAIN),
            total: 9,
            unread: 0,
        });
        for id in 1..=7 {
            fixture.insert(&Row::new(id, fixture::MAIN_INBOX, fixture::T));
        }
        fixture.insert(&Row::new(8, fixture::MAIN_INBOX, fixture::T - 1));
        fixture.insert(&Row::new(9, fixture::MAIN_INBOX, fixture::T + 1));

        let store = MailStore::new(fixture.config());
        let reader = store.open().unwrap();
        let mut seen = Vec::new();
        let mut sizes = Vec::new();
        let mut cursor = None;
        loop {
            let page = reader
                .messages(&MessageQuery {
                    limit: 3,
                    cursor,
                    ..MessageQuery::default()
                })
                .unwrap();
            sizes.push(page.messages.len());
            seen.extend(ids(&page));
            let Some(next) = page.next_cursor else { break };
            let next = Cursor::decode(&next.encode()).unwrap();
            cursor = Some(next);
        }
        assert_eq!(seen, [9, 7, 6, 5, 4, 3, 2, 1, 8]);
        assert_eq!(sizes, [3, 3, 3]);

        let page = reader
            .messages(&MessageQuery {
                limit: 9,
                ..MessageQuery::default()
            })
            .unwrap();
        assert_eq!(page.next_cursor, None);
        let page = reader
            .messages(&MessageQuery {
                limit: 8,
                ..MessageQuery::default()
            })
            .unwrap();
        assert_eq!(
            page.next_cursor,
            Some(Cursor {
                date_received: fixture::T,
                id: 1
            })
        );
    }

    #[test]
    fn messages_page_through_rows_without_a_date() {
        let fixture = Fixture::empty();
        fixture.mailbox(&MailboxRow {
            id: fixture::MAIN_INBOX,
            url: format!("ews://{}/Inbox", fixture::MAIN),
            total: 4,
            unread: 0,
        });
        for id in 1..=4 {
            fixture.insert(&Row::new(id, fixture::MAIN_INBOX, fixture::T + id));
        }
        fixture
            .writer()
            .execute(
                "UPDATE messages SET date_received = NULL WHERE ROWID IN (2, 3)",
                [],
            )
            .unwrap();

        let store = MailStore::new(fixture.config());
        let reader = store.open().unwrap();
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = reader
                .messages(&MessageQuery {
                    limit: 1,
                    cursor,
                    ..MessageQuery::default()
                })
                .unwrap();
            seen.extend(ids(&page));
            let Some(next) = page.next_cursor else { break };
            cursor = Some(Cursor::decode(&next.encode()).unwrap());
        }
        assert_eq!(seen, [4, 1, 3, 2]);
    }

    #[test]
    fn messages_clamp_the_limit() {
        let fixture = Fixture::standard();
        let one = list(
            &fixture,
            MessageQuery {
                limit: 0,
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(one, [fixture::MULTIPART_ALL_MAIL]);
        let all = list(
            &fixture,
            MessageQuery {
                limit: u32::MAX,
                ..MessageQuery::default()
            },
        )
        .unwrap();
        assert_eq!(all, ALL_VISIBLE);
    }

    #[test]
    fn cursor_round_trips_as_url_safe_base64() {
        let cursor = Cursor {
            date_received: fixture::T,
            id: 383_621,
        };
        let encoded = cursor.encode();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&encoded).unwrap(),
            b"1791200000:383621"
        );
        assert_eq!(Cursor::decode(&encoded), Ok(cursor));
        assert_eq!(
            serde_json::to_value(cursor).unwrap(),
            serde_json::Value::String(encoded)
        );

        let cursor = Cursor {
            date_received: -1,
            id: 7,
        };
        assert_eq!(Cursor::decode(&cursor.encode()), Ok(cursor));
    }

    #[test]
    fn cursor_refuses_undecodable_text() {
        let mut cases = vec!["".to_owned(), "!!!".to_owned(), "a b".to_owned()];
        for text in [
            "1791200000",
            "1791200000:",
            ":5",
            "x:5",
            "-:5",
            "--1:5",
            "+1:5",
            "1:0",
            "1:-5",
            "1:5:6",
            "1:05",
        ] {
            cases.push(URL_SAFE_NO_PAD.encode(text));
        }
        cases.push(URL_SAFE_NO_PAD.encode([0xff, b':', b'1']));
        for text in cases {
            assert_eq!(Cursor::decode(&text), Err(CursorError), "{text:?}");
        }
    }

    #[test]
    fn message_reads_a_plain_text_body() {
        let fixture = Fixture::standard();
        let message = get(&fixture, fixture::PLAIN).unwrap();
        assert_eq!(message.summary.subject.as_deref(), Some("Quarterly report"));
        assert_eq!(
            message.cc,
            vec![Address {
                name: Some("Carol".to_owned()),
                address: "carol@example.com".to_owned(),
            }]
        );
        assert_eq!(message.summary.to.len(), 1);
        assert_eq!(
            message.body.as_deref().map(str::trim),
            Some("Plain version")
        );
        assert!(!message.body_truncated);
        assert!(!message.partial);
        assert!(message.attachments.is_empty());
        assert!(message.summary.has_body);
    }

    #[test]
    fn message_converts_an_html_only_body() {
        let fixture = Fixture::standard();
        let message = get(&fixture, fixture::HTML).unwrap();
        let body = message.body.unwrap();
        assert!(body.contains("Big sale"), "{body:?}");
        assert!(!body.contains("<h1>"), "{body:?}");
        assert!(message.summary.flagged);
    }

    #[test]
    fn message_reads_a_non_ascii_multipart_message_with_an_attachment() {
        let fixture = Fixture::standard();
        for id in [fixture::MULTIPART, fixture::MULTIPART_ALL_MAIL] {
            let message = get(&fixture, id).unwrap();
            assert_eq!(
                message.summary.subject.as_deref(),
                Some(fixture::CYRILLIC_SUBJECT)
            );
            assert_eq!(
                message.body.as_deref().map(str::trim),
                Some(fixture::CYRILLIC_BODY)
            );
            assert_eq!(
                message.attachments,
                vec![Attachment {
                    name: Some("report.pdf".to_owned()),
                    content_type: "application/pdf".to_owned(),
                    size: fixture::ATTACHMENT_BYTES.len(),
                }]
            );
        }
        let all_mail = get(&fixture, fixture::MULTIPART_ALL_MAIL).unwrap();
        assert_eq!(all_mail.summary.mailbox, "[Gmail]/All Mail");
    }

    #[test]
    fn message_marks_a_partial_file() {
        let fixture = Fixture::standard();
        let message = get(&fixture, fixture::PARTIAL).unwrap();
        assert!(message.partial);
        assert!(message.summary.has_body);
        let body = message.body.unwrap();
        assert!(body.contains("The first lines"), "{body:?}");
    }

    #[test]
    fn message_without_a_file_has_no_body() {
        let fixture = Fixture::standard();
        let message = get(&fixture, fixture::MISSING).unwrap();
        assert!(!message.summary.has_body);
        assert_eq!(message.body, None);
        assert!(!message.partial);
        assert!(message.attachments.is_empty());
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["body"], serde_json::Value::Null);
        assert_eq!(json["id"], fixture::MISSING);
        assert_eq!(json["has_body"], false);
    }

    #[test]
    fn message_hides_every_invisible_row() {
        let fixture = Fixture::standard();
        for id in [
            fixture::IN_DELETED_ITEMS,
            fixture::DELETED_ROW,
            fixture::UNCONFIGURED,
            fixture::IN_SPAM,
            fixture::IN_CRAFTED,
            999_999,
        ] {
            assert_eq!(get(&fixture, id), None, "{id}");
        }
    }

    #[test]
    fn a_mailbox_linked_outside_the_root_is_refused() {
        let fixture = Fixture::standard();
        let outside = tempfile::tempdir().unwrap();
        let inbox = fixture.root.join(fixture::MAIN).join("Inbox.mbox");
        fs::rename(&inbox, outside.path().join("Inbox.mbox")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("Inbox.mbox"), &inbox).unwrap();

        let store = MailStore::new(fixture.config());
        let reader = store.open().unwrap();
        let page = reader
            .messages(&MessageQuery {
                accounts: vec![name("main")],
                ..MessageQuery::default()
            })
            .unwrap();
        for message in &page.messages {
            assert!(!message.has_body, "{}", message.id);
        }
        let result = reader.message(message_id(fixture::PLAIN));
        let Err(StoreError::File(EmlxError::OutsideRoot(_))) = result else {
            panic!("{result:?}");
        };
    }

    #[test]
    fn the_reader_answers_while_a_writer_holds_a_transaction() {
        let fixture = Fixture::standard();
        let writer = fixture.writer();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute(
                "INSERT INTO messages (ROWID, subject, date_received, mailbox) VALUES (9000, 1, ?1, ?2)",
                [fixture::T + 900, fixture::MAIN_INBOX],
            )
            .unwrap();
        let index = fixture.root.join(MAIL_INDEX_RELATIVE_PATH);
        assert!(index.with_file_name("Envelope Index-wal").exists());
        assert!(index.with_file_name("Envelope Index-shm").exists());

        let started = Instant::now();
        let listed = list(&fixture, MessageQuery::default()).unwrap();
        assert!(started.elapsed() < BUSY_TIMEOUT, "{:?}", started.elapsed());
        assert_eq!(listed, ALL_VISIBLE);

        writer.execute_batch("COMMIT").unwrap();
        let listed = list(&fixture, MessageQuery::default()).unwrap();
        assert_eq!(listed[0], 9000);
    }

    #[test]
    fn the_reader_opens_a_wal_index_without_sidecars() {
        let fixture = Fixture::standard();
        let index = fixture.root.join(MAIL_INDEX_RELATIVE_PATH);
        assert!(!index.with_file_name("Envelope Index-wal").exists());
        assert!(!index.with_file_name("Envelope Index-shm").exists());
        let journal: String = fixture
            .writer()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(
            list(&fixture, MessageQuery::default()).unwrap(),
            ALL_VISIBLE
        );
    }

    #[test]
    fn the_reader_cannot_write() {
        let fixture = Fixture::standard();
        let store = MailStore::new(fixture.config());
        let reader = store.open().unwrap();
        let result = reader.conn.execute("DELETE FROM messages", []);
        assert!(result.is_err());
        let query_only: bool = reader
            .conn
            .query_row("PRAGMA query_only", [], |row| row.get(0))
            .unwrap();
        assert!(query_only);
        assert_eq!(
            list(&fixture, MessageQuery::default()).unwrap(),
            ALL_VISIBLE
        );
    }

    #[test]
    fn ufold_folds_beyond_ascii_and_keeps_null() {
        let fixture = Fixture::empty();
        let index = fixture.root.join(MAIL_INDEX_RELATIVE_PATH);
        let conn = open_index(&index).unwrap();
        let folded: String = conn
            .query_row("SELECT ufold('ПРИВЕТ Straße ÉTÉ')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(folded, "привет straße été");
        let null: Option<String> = conn
            .query_row("SELECT ufold(NULL)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(null, None);
    }

    #[test]
    fn open_reports_a_missing_root() {
        let fixture = Fixture::empty();
        let mut config = fixture.config();
        config.root = Some(fixture.root.join("missing"));
        let result = MailStore::new(config).open().map(|_| ());
        let Err(StoreError::RootAccess { .. }) = result else {
            panic!("{result:?}");
        };
    }

    #[test]
    fn open_reports_a_missing_index() {
        let fixture = Fixture::empty();
        fs::remove_file(fixture.root.join(MAIL_INDEX_RELATIVE_PATH)).unwrap();
        let result = MailStore::new(fixture.config()).open().map(|_| ());
        let Err(StoreError::Open(_)) = result else {
            panic!("{result:?}");
        };
    }

    #[test]
    fn a_corrupt_index_fails_the_query() {
        let fixture = Fixture::empty();
        fs::write(
            fixture.root.join(MAIL_INDEX_RELATIVE_PATH),
            b"not a database at all, just text that is long enough to be read as a header",
        )
        .unwrap();
        let store = MailStore::new(fixture.config());
        let reader = store.open().unwrap();
        let result = reader.accounts();
        let Err(StoreError::Query(_)) = result else {
            panic!("{result:?}");
        };
    }

    #[test]
    fn no_configured_account_shows_nothing() {
        let fixture = Fixture::standard();
        let mut config = fixture.config();
        config.accounts = Vec::new();
        let store = MailStore::new(config);
        let reader = store.open().unwrap();
        assert_eq!(reader.accounts().unwrap(), []);
        let page = reader.messages(&MessageQuery::default()).unwrap();
        assert_eq!(ids(&page), [0_i64; 0]);
        assert_eq!(page.next_cursor, None);
        for id in ALL_VISIBLE {
            assert_eq!(reader.message(message_id(id)).unwrap(), None, "{id}");
        }
        assert_eq!(
            store.status(Utc::now()),
            Ok(MailStatus {
                accounts: 0,
                newest_message_age_s: None,
            })
        );
    }
    #[test]
    fn status_reports_accounts_and_the_newest_visible_message() {
        let fixture = Fixture::standard();
        let store = MailStore::new(fixture.config());
        let now = DateTime::<Utc>::from_timestamp(fixture::T + 1_000, 0).unwrap();
        assert_eq!(
            store.status(now),
            Ok(MailStatus {
                accounts: 2,
                newest_message_age_s: Some(700),
            })
        );
        let past = DateTime::<Utc>::from_timestamp(fixture::T, 0).unwrap();
        assert_eq!(
            store.status(past).map(|status| status.newest_message_age_s),
            Ok(Some(0))
        );
    }

    #[test]
    fn status_without_messages_has_no_age() {
        let fixture = Fixture::empty();
        for (id, url) in [
            (1, format!("ews://{}/Inbox", fixture::MAIN)),
            (2, format!("imap://{}/INBOX", fixture::GMAIL)),
        ] {
            fixture.mailbox(&MailboxRow {
                id,
                url,
                total: 0,
                unread: 0,
            });
        }
        let store = MailStore::new(fixture.config());
        assert_eq!(
            store.status(Utc::now()),
            Ok(MailStatus {
                accounts: 2,
                newest_message_age_s: None,
            })
        );
    }

    #[test]
    fn status_reports_an_unreadable_store() {
        let fixture = Fixture::standard();
        let mut config = fixture.config();
        config.root = Some(fixture.root.join("missing"));
        assert_eq!(
            MailStore::new(config).status(Utc::now()),
            Err(MailProblem::NoAccess)
        );
        fs::write(
            fixture.root.join(MAIL_INDEX_RELATIVE_PATH),
            b"not a database at all, just text that is long enough to be read as a header",
        )
        .unwrap();
        assert_eq!(
            MailStore::new(fixture.config()).status(Utc::now()),
            Err(MailProblem::NoAccess)
        );
    }

    #[test]
    fn status_reports_a_missing_table_or_column() {
        for change in [
            "ALTER TABLE messages DROP COLUMN flagged",
            "DROP TABLE summaries",
            "ALTER TABLE mailboxes RENAME COLUMN url TO address",
        ] {
            let fixture = Fixture::standard();
            fixture.writer().execute_batch(change).unwrap();
            assert_eq!(
                MailStore::new(fixture.config()).status(Utc::now()),
                Err(MailProblem::SchemaChanged),
                "{change}"
            );
        }
    }

    #[test]
    fn status_reports_a_configured_account_without_mailboxes() {
        let fixture = Fixture::standard();
        let mut config = fixture.config();
        config.accounts.push(MailAccount {
            id: AccountId::parse("11111111-2222-3333-4444-555555555555").unwrap(),
            name: name("gone"),
        });
        assert_eq!(
            MailStore::new(config).status(Utc::now()),
            Err(MailProblem::AccountMissing)
        );
    }

    fn listing(fixture: &Fixture) -> Vec<AccountListing> {
        MailStore::new(fixture.config())
            .open()
            .unwrap()
            .listing()
            .unwrap()
    }

    #[test]
    fn listing_counts_every_account_with_its_registration() {
        let fixture = Fixture::standard();
        fixture.register_accounts(&[
            (fixture::MAIN, "com.apple.account.Exchange", "Work"),
            (
                &fixture::GMAIL.to_lowercase(),
                "com.apple.account.IMAP",
                "me@gmail.example",
            ),
        ]);
        let listed = listing(&fixture);
        let newest = |seconds| Some(local_time(fixture::T + seconds));
        assert_eq!(
            listed,
            vec![
                AccountListing {
                    id: AccountId::parse(fixture::OTHER).unwrap(),
                    kind: AccountKind::Imap,
                    registered: None,
                    mailboxes: 1,
                    messages: 1,
                    newest: newest(600),
                    name: None,
                },
                AccountListing {
                    id: AccountId::parse(fixture::MAIN).unwrap(),
                    kind: AccountKind::Exchange,
                    registered: Some(RegisteredAccount {
                        account_type: Some("com.apple.account.Exchange".to_owned()),
                        description: Some("Work".to_owned()),
                    }),
                    mailboxes: 2,
                    messages: 5,
                    newest: newest(400),
                    name: Some(name("main")),
                },
                AccountListing {
                    id: AccountId::parse(fixture::GMAIL).unwrap(),
                    kind: AccountKind::Imap,
                    registered: Some(RegisteredAccount {
                        account_type: Some("com.apple.account.IMAP".to_owned()),
                        description: Some("me@gmail.example".to_owned()),
                    }),
                    mailboxes: 3,
                    messages: 3,
                    newest: newest(700),
                    name: Some(name("gmail")),
                },
            ]
        );
    }

    #[test]
    fn listing_without_accounts_database_has_no_registration() {
        let fixture = Fixture::standard();
        let listed = listing(&fixture);
        assert_eq!(listed.len(), 3);
        for account in &listed {
            assert_eq!(account.registered, None, "{}", account.id);
        }
        let fixture = Fixture::empty();
        fixture.mailbox(&MailboxRow {
            id: 1,
            url: format!("local://{}/Archive", fixture::MAIN),
            total: 0,
            unread: 0,
        });
        let listed = listing(&fixture);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, AccountKind::Local);
        assert_eq!(listed[0].messages, 0);
        assert_eq!(listed[0].newest, None);
    }

    #[test]
    fn account_kind_displays_its_scheme_name() {
        assert_eq!(AccountKind::Exchange.to_string(), "exchange");
        assert_eq!(AccountKind::Imap.to_string(), "imap");
        assert_eq!(AccountKind::Local.to_string(), "local");
    }

    fn place(config: MailConfig, id: i64) -> Option<MessagePlace> {
        let store = MailStore::new(config);
        store.open().unwrap().place(message_id(id)).unwrap()
    }

    fn add_mailbox(fixture: &Fixture, id: i64, url: String) {
        fixture.mailbox(&MailboxRow {
            id,
            url,
            total: 0,
            unread: 0,
        });
    }

    fn target(path: &str, visibility: Visibility) -> Option<MoveTarget> {
        Some(MoveTarget {
            path: path.to_owned(),
            visibility,
        })
    }

    fn account(id: &str, value: &str) -> MailAccount {
        MailAccount {
            id: AccountId::parse(id).unwrap(),
            name: name(value),
        }
    }

    #[test]
    fn place_finds_the_gmail_spam_mailbox_and_inbox() {
        let fixture = Fixture::standard();
        assert_eq!(
            place(fixture.config(), fixture::MULTIPART),
            Some(MessagePlace {
                account: account(fixture::GMAIL, "gmail"),
                mailbox: "INBOX".to_owned(),
                junk: target("[Gmail]/Spam", Visibility::Excluded),
                inbox: target("INBOX", Visibility::Visible),
            })
        );
        let found = place(fixture.config(), fixture::MULTIPART_ALL_MAIL).unwrap();
        assert_eq!(found.mailbox, "[Gmail]/All Mail");
        assert_eq!(found.junk, target("[Gmail]/Spam", Visibility::Excluded));
    }

    #[test]
    fn place_finds_the_exchange_junk_email_mailbox_and_inbox() {
        let fixture = Fixture::standard();
        add_mailbox(
            &fixture,
            20,
            format!("ews://{}/Junk%20Email", fixture::MAIN),
        );
        assert_eq!(
            place(fixture.config(), fixture::PLAIN),
            Some(MessagePlace {
                account: account(fixture::MAIN, "main"),
                mailbox: "Inbox".to_owned(),
                junk: target("Junk Email", Visibility::Excluded),
                inbox: target("Inbox", Visibility::Visible),
            })
        );
    }

    #[test]
    fn place_finds_the_icloud_junk_mailbox() {
        let fixture = Fixture::standard();
        add_mailbox(&fixture, 20, format!("imap://{}/Junk", fixture::OTHER));
        let mut config = fixture.config();
        config.accounts.push(account(fixture::OTHER, "icloud"));
        assert_eq!(
            place(config, fixture::UNCONFIGURED),
            Some(MessagePlace {
                account: account(fixture::OTHER, "icloud"),
                mailbox: "INBOX".to_owned(),
                junk: target("Junk", Visibility::Excluded),
                inbox: target("INBOX", Visibility::Visible),
            })
        );
    }

    #[test]
    fn place_without_a_junk_mailbox_has_no_junk_target() {
        let fixture = Fixture::standard();
        let found = place(fixture.config(), fixture::PLAIN).unwrap();
        assert_eq!(found.junk, None);
        assert_eq!(found.inbox, target("Inbox", Visibility::Visible));
    }

    #[test]
    fn place_without_an_inbox_has_no_inbox_target() {
        let fixture = Fixture::empty();
        add_mailbox(
            &fixture,
            1,
            format!("imap://{}/%5BGmail%5D/All%20Mail", fixture::GMAIL),
        );
        add_mailbox(&fixture, 2, format!("imap://{}/INBOX", fixture::OTHER));
        add_mailbox(&fixture, 3, format!("imap://{}/Spam", fixture::OTHER));
        fixture.insert(&Row::new(10, 1, fixture::T));
        let found = place(fixture.config(), 10).unwrap();
        assert_eq!(found.mailbox, "[Gmail]/All Mail");
        assert_eq!(found.junk, None);
        assert_eq!(found.inbox, None);
    }

    #[test]
    fn place_prefers_the_earlier_junk_candidate_in_its_stored_case() {
        let fixture = Fixture::standard();
        add_mailbox(&fixture, 20, format!("ews://{}/SPAM", fixture::MAIN));
        add_mailbox(&fixture, 21, format!("ews://{}/junk", fixture::MAIN));
        let found = place(fixture.config(), fixture::PLAIN).unwrap();
        assert_eq!(found.junk, target("junk", Visibility::Excluded));
        add_mailbox(
            &fixture,
            22,
            format!("ews://{}/Junk%20E-mail", fixture::MAIN),
        );
        let found = place(fixture.config(), fixture::PLAIN).unwrap();
        assert_eq!(found.junk, target("Junk E-mail", Visibility::Visible));
    }

    #[test]
    fn place_reports_a_target_left_visible_by_the_config() {
        let fixture = Fixture::standard();
        let mut config = fixture.config();
        config.exclude_mailboxes = Vec::new();
        let found = place(config.clone(), fixture::IN_SPAM).unwrap();
        assert_eq!(found.mailbox, "[Gmail]/Spam");
        assert_eq!(found.junk, target("[Gmail]/Spam", Visibility::Visible));
        assert_eq!(found.inbox, target("INBOX", Visibility::Visible));
        config.exclude_mailboxes = vec!["inbox".to_owned()];
        let found = place(config, fixture::MULTIPART_ALL_MAIL).unwrap();
        assert_eq!(found.inbox, target("INBOX", Visibility::Excluded));
    }

    #[test]
    fn place_hides_every_invisible_row() {
        let fixture = Fixture::standard();
        for id in [
            fixture::IN_DELETED_ITEMS,
            fixture::DELETED_ROW,
            fixture::UNCONFIGURED,
            fixture::IN_SPAM,
            fixture::IN_CRAFTED,
            999_999,
        ] {
            assert_eq!(place(fixture.config(), id), None, "{id}");
        }
        let mut config = fixture.config();
        config.accounts = Vec::new();
        assert_eq!(place(config, fixture::PLAIN), None);
    }
}
