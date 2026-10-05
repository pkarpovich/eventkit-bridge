use std::fs;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, params};
use tempfile::TempDir;

use crate::config::{
    AccountId, AccountName, DEFAULT_EXCLUDED_MAILBOXES, MAIL_INDEX_RELATIVE_PATH, MailAccount,
    MailConfig,
};
use crate::mail::MessageId;
use crate::mail::emlx::{Completeness, partition};

pub const MAIN: &str = "6F1C4A52-0B7E-4D8A-9C3B-2E5F7A1D9B40";
pub const GMAIL: &str = "A3D9E1F7-5C2B-4E6A-8F10-7B4C3D2E1A90";
pub const OTHER: &str = "0D4B8C2E-6A1F-4B3D-9E7C-5F2A8D1B6C30";
pub const STORE: &str = "5E0A9B1C-3D7F-4A26-8C4E-1B9D2F6A7E35";

pub const T: i64 = 1_791_200_000;

pub const MAIN_INBOX: i64 = 1;
pub const MAIN_DELETED: i64 = 2;
pub const GMAIL_INBOX: i64 = 3;
pub const GMAIL_ALL: i64 = 4;
pub const OTHER_INBOX: i64 = 5;
pub const GMAIL_SPAM: i64 = 6;
pub const GMAIL_CRAFTED: i64 = 7;

pub const PLAIN: i64 = 830;
pub const HTML: i64 = 12_345;
pub const MULTIPART: i64 = 383_621;
pub const MULTIPART_ALL_MAIL: i64 = 383_622;
pub const PARTIAL: i64 = 1_500;
pub const MISSING: i64 = 1_501;
pub const IN_DELETED_ITEMS: i64 = 2_000;
pub const DELETED_ROW: i64 = 2_001;
pub const UNCONFIGURED: i64 = 2_002;
pub const IN_SPAM: i64 = 2_003;
pub const IN_CRAFTED: i64 = 2_004;

pub const CYRILLIC_SUBJECT: &str = "Привет из Минска";
pub const CYRILLIC_BODY: &str = "Встреча в пятницу в 10:00";
pub const ATTACHMENT_BYTES: &[u8] = b"%PDF-1.4 fake report bytes";

pub const PARTIAL_MESSAGE: &str = "Subject: Long thread\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThe first lines of a long message\r\nthat Mail only half downloaded";

pub struct Fixture {
    _dir: TempDir,
    pub root: PathBuf,
}

pub struct FileSpec<'a> {
    pub account: &'a str,
    pub mailbox: &'a [&'a str],
    pub id: MessageId,
    pub completeness: Completeness,
}

pub struct MailboxRow {
    pub id: i64,
    pub url: String,
    pub total: i64,
    pub unread: i64,
}

pub struct Row<'a> {
    pub id: i64,
    pub mailbox: i64,
    pub date: i64,
    pub from: (&'a str, &'a str),
    pub subject: &'a str,
    pub summary: Option<&'a str>,
    pub read: bool,
    pub flagged: bool,
    pub deleted: bool,
    pub to: &'a [(&'a str, &'a str)],
    pub cc: &'a [(&'a str, &'a str)],
}

impl Row<'_> {
    pub fn new(id: i64, mailbox: i64, date: i64) -> Row<'static> {
        Row {
            id,
            mailbox,
            date,
            from: ("Sender", "sender@example.com"),
            subject: "Subject",
            summary: None,
            read: true,
            flagged: false,
            deleted: false,
            to: &[],
            cc: &[],
        }
    }
}

pub fn inbox(id: i64, completeness: Completeness) -> FileSpec<'static> {
    FileSpec {
        account: MAIN,
        mailbox: &["Inbox"],
        id: MessageId::new(id).unwrap(),
        completeness,
    }
}

pub fn emlx_bytes(message: &str) -> Vec<u8> {
    let mut bytes = format!("{}\n{message}", message.len()).into_bytes();
    bytes.extend_from_slice(
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>flags</key><integer>8590195713</integer></dict></plist>\n",
    );
    bytes
}

pub fn plain_message() -> String {
    "From: Alice Example <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nCc: Carol <carol@example.com>\r\nSubject: Quarterly report\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain version\r\n".to_owned()
}

pub fn alternative_message() -> String {
    "Subject: Both\r\nContent-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n--alt\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain version\r\n--alt\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>HTML version</p>\r\n--alt--\r\n".to_owned()
}

pub fn html_message() -> String {
    "From: Newsletter <news@shop.example>\r\nSubject: Weekly deals\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Big sale</h1><p>Everything <b>50%</b> off</p></body></html>\r\n".to_owned()
}

pub fn multipart_message() -> String {
    let subject = STANDARD.encode(CYRILLIC_SUBJECT);
    let body = STANDARD.encode(CYRILLIC_BODY);
    let attachment = STANDARD.encode(ATTACHMENT_BYTES);
    format!(
        "From: =?UTF-8?B?{name}?= <ivan@example.ru>\r\nTo: me@gmail.example\r\nSubject: =?UTF-8?B?{subject}?=\r\nContent-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n--mix\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{body}\r\n--mix\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{attachment}\r\n--mix--\r\n",
        name = STANDARD.encode("Иван Петров"),
    )
}

impl Fixture {
    pub fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("V10");
        let index = root.join(MAIL_INDEX_RELATIVE_PATH);
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        let conn = Connection::open(&index).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(include_str!("../../fixtures/mail_schema.sql"))
            .unwrap();
        Self { _dir: dir, root }
    }

    pub fn standard() -> Self {
        let fixture = Self::empty();
        let mailboxes = [
            (MAIN_INBOX, format!("ews://{MAIN}/Inbox"), 4, 2),
            (MAIN_DELETED, format!("ews://{MAIN}/Deleted%20Items"), 1, 0),
            (GMAIL_INBOX, format!("imap://{GMAIL}/INBOX"), 1, 1),
            (
                GMAIL_ALL,
                format!("imap://{GMAIL}/%5BGmail%5D/All%20Mail"),
                1,
                1,
            ),
            (OTHER_INBOX, format!("imap://{OTHER}/INBOX"), 1, 0),
            (GMAIL_SPAM, format!("imap://{GMAIL}/%5BGmail%5D/Spam"), 1, 0),
            (
                GMAIL_CRAFTED,
                format!("imap://{GMAIL}/INBOX/..%2F..%2Fetc"),
                1,
                0,
            ),
        ];
        for (id, url, total, unread) in mailboxes {
            fixture.mailbox(&MailboxRow {
                id,
                url,
                total,
                unread,
            });
        }

        fixture.insert(&Row {
            from: ("Alice Example", "alice@example.com"),
            subject: "Quarterly report",
            summary: Some("Numbers attached"),
            to: &[("Bob", "bob@example.com")],
            cc: &[("Carol", "carol@example.com")],
            ..Row::new(PLAIN, MAIN_INBOX, T + 100)
        });
        fixture.write_emlx(inbox(PLAIN, Completeness::Full), &plain_message());

        fixture.insert(&Row {
            from: ("Newsletter", "news@shop.example"),
            subject: "Weekly deals",
            read: false,
            flagged: true,
            to: &[("Bob", "bob@example.com")],
            ..Row::new(HTML, MAIN_INBOX, T + 200)
        });
        fixture.write_emlx(inbox(HTML, Completeness::Full), &html_message());

        for (id, mailbox, path) in [
            (MULTIPART, GMAIL_INBOX, &["INBOX"][..]),
            (MULTIPART_ALL_MAIL, GMAIL_ALL, &["[Gmail]", "All Mail"][..]),
        ] {
            fixture.insert(&Row {
                from: ("Иван Петров", "ivan@example.ru"),
                subject: CYRILLIC_SUBJECT,
                summary: Some(CYRILLIC_BODY),
                read: false,
                to: &[("", "me@gmail.example")],
                ..Row::new(id, mailbox, T + 300)
            });
            fixture.write_emlx(
                FileSpec {
                    account: GMAIL,
                    mailbox: path,
                    id: MessageId::new(id).unwrap(),
                    completeness: Completeness::Full,
                },
                &multipart_message(),
            );
        }

        fixture.insert(&Row::new(PARTIAL, MAIN_INBOX, T + 50));
        fixture.write_emlx(inbox(PARTIAL, Completeness::Partial), PARTIAL_MESSAGE);

        fixture.insert(&Row {
            read: false,
            ..Row::new(MISSING, MAIN_INBOX, T + 40)
        });

        fixture.insert(&Row::new(IN_DELETED_ITEMS, MAIN_DELETED, T + 400));
        fixture.insert(&Row {
            deleted: true,
            ..Row::new(DELETED_ROW, MAIN_INBOX, T + 500)
        });
        fixture.insert(&Row::new(UNCONFIGURED, OTHER_INBOX, T + 600));
        fixture.insert(&Row::new(IN_SPAM, GMAIL_SPAM, T + 700));
        fixture.insert(&Row::new(IN_CRAFTED, GMAIL_CRAFTED, T + 800));
        for (id, account, path) in [
            (IN_DELETED_ITEMS, MAIN, &["Deleted Items"][..]),
            (DELETED_ROW, MAIN, &["Inbox"][..]),
            (UNCONFIGURED, OTHER, &["INBOX"][..]),
            (IN_SPAM, GMAIL, &["[Gmail]", "Spam"][..]),
        ] {
            fixture.write_emlx(
                FileSpec {
                    account,
                    mailbox: path,
                    id: MessageId::new(id).unwrap(),
                    completeness: Completeness::Full,
                },
                &plain_message(),
            );
        }
        fixture
    }

    pub fn writer(&self) -> Connection {
        Connection::open(self.root.join(MAIL_INDEX_RELATIVE_PATH)).unwrap()
    }

    pub fn config(&self) -> MailConfig {
        let mut exclude_mailboxes = Vec::new();
        for path in DEFAULT_EXCLUDED_MAILBOXES {
            exclude_mailboxes.push(path.to_owned());
        }
        MailConfig {
            accounts: vec![
                MailAccount {
                    id: AccountId::parse(GMAIL).unwrap(),
                    name: AccountName::parse("gmail").unwrap(),
                },
                MailAccount {
                    id: AccountId::parse(MAIN).unwrap(),
                    name: AccountName::parse("main").unwrap(),
                },
            ],
            exclude_mailboxes,
            root: Some(self.root.clone()),
        }
    }

    pub fn mailbox(&self, row: &MailboxRow) {
        let MailboxRow {
            id,
            url,
            total,
            unread,
        } = row;
        self.writer()
            .execute(
                "INSERT INTO mailboxes (ROWID, url, total_count, unread_count) VALUES (?1, ?2, ?3, ?4)",
                params![id, url, total, unread],
            )
            .unwrap();
    }

    pub fn insert(&self, row: &Row<'_>) {
        let Row {
            id,
            mailbox,
            date,
            from,
            subject,
            summary,
            read,
            flagged,
            deleted,
            to,
            cc,
        } = row;
        let conn = self.writer();
        let sender = address(&conn, *from);
        conn.execute("INSERT INTO subjects (subject) VALUES (?1)", [subject])
            .unwrap();
        let subject = conn.last_insert_rowid();
        let summary = match summary {
            Some(summary) => {
                conn.execute("INSERT INTO summaries (summary) VALUES (?1)", [summary])
                    .unwrap();
                Some(conn.last_insert_rowid())
            }
            None => None,
        };
        conn.execute(
            "INSERT INTO messages (ROWID, sender, subject, summary, date_sent, date_received, mailbox, read, flagged, deleted, size, conversation_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1000, ?1)",
            params![id, sender, subject, summary, date - 5, date, mailbox, read, flagged, deleted],
        )
        .unwrap();
        for (kind, recipients) in [(0, to), (1, cc)] {
            for (position, recipient) in recipients.iter().enumerate() {
                let recipient = address(&conn, *recipient);
                conn.execute(
                    "INSERT INTO recipients (message, address, type, position) VALUES (?1, ?2, ?3, ?4)",
                    params![id, recipient, kind, i64::try_from(position).unwrap()],
                )
                .unwrap();
            }
        }
    }

    pub fn write_emlx(&self, spec: FileSpec<'_>, message: &str) {
        let FileSpec {
            account,
            mailbox,
            id,
            completeness,
        } = spec;
        let mut dir = self.root.join(account);
        for segment in mailbox {
            dir.push(format!("{segment}.mbox"));
        }
        let dir = dir.join(STORE).join(partition(id));
        fs::create_dir_all(&dir).unwrap();
        let name = match completeness {
            Completeness::Full => format!("{id}.emlx"),
            Completeness::Partial => format!("{id}.partial.emlx"),
        };
        fs::write(dir.join(name), emlx_bytes(message)).unwrap();
    }
}

fn address(conn: &Connection, (name, address): (&str, &str)) -> i64 {
    conn.execute(
        "INSERT INTO addresses (address, comment) VALUES (?1, ?2)",
        [address, name],
    )
    .unwrap();
    conn.last_insert_rowid()
}
