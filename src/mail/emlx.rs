use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use mail_parser::{MessageParser, MessagePart, MessagePartId, MimeHeaders, PartType};
use serde::Serialize;

use crate::config::{AccountId, is_decimal, is_full_uuid};
use crate::mail::{MailboxPath, MessageId};

/// The most body characters returned before the body is cut.
pub const BODY_CAP: usize = 100_000;
const HTML_WIDTH: usize = 1_000;
const MBOX_SUFFIX: &str = ".mbox";
const PARTITION_SIZE: i64 = 1_000;
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// The mail root, canonicalised so every message path can be checked against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailRoot(PathBuf);

impl MailRoot {
    /// Canonicalises `path`, which must exist.
    pub fn new(path: &Path) -> io::Result<Self> {
        Ok(Self(path.canonicalize()?))
    }

    /// Returns the canonical root.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The directory of a mailbox: `<root>/<account>/<segment>.mbox/...`.
    pub fn mailbox_dir(&self, account: &AccountId, mailbox: &MailboxPath) -> PathBuf {
        let mut dir = self.0.join(account.as_str());
        for segment in mailbox.segments() {
            dir.push(format!("{segment}{MBOX_SUFFIX}"));
        }
        dir
    }
}

/// Whether Mail holds the whole message or only part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// `<id>.emlx` holds the whole message.
    Full,
    /// Only `<id>.partial.emlx` exists: headers and part of the body.
    Partial,
}

/// A message file found on disk, canonical and under the mail root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmlxFile {
    /// The canonical path of the file.
    pub path: PathBuf,
    /// Which of the two file names was found.
    pub completeness: Completeness,
}

/// Why a message file could not be found or read.
#[derive(Debug, thiserror::Error)]
pub enum EmlxError {
    /// A directory or file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The canonical path of a message file leaves the mail root.
    #[error("message file {} is outside the mail root", .0.display())]
    OutsideRoot(PathBuf),
    /// The first line is not the byte length of the message.
    #[error("the first line of an .emlx file is not a byte length")]
    LengthLine,
}

/// The parts of a message the bridge serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    /// The plain-text body, possibly cut at [`BODY_CAP`] characters; empty when the message has no text part.
    pub body: String,
    /// Whether `body` was cut.
    pub body_truncated: bool,
    /// The attachments, without their contents.
    pub attachments: Vec<Attachment>,
}

/// An attachment as listed to clients; its bytes are never served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attachment {
    /// The file name, when the message gives one.
    pub name: Option<String>,
    /// The MIME type, lowercase.
    pub content_type: String,
    /// The decoded size in bytes.
    pub size: usize,
}

/// The directory of message `id` relative to a store directory: `Data/<digits of id / 1000, reversed>/Messages`.
pub fn partition(id: MessageId) -> PathBuf {
    let mut dir = PathBuf::from("Data");
    let mut rest = id.get() / PARTITION_SIZE;
    while rest > 0 {
        dir.push((rest % 10).to_string());
        rest /= 10;
    }
    dir.push("Messages");
    dir
}

/// Finds the file of message `id` in `mailbox`, trying `<id>.emlx` then `<id>.partial.emlx` in each store directory.
///
/// A file whose canonical path leaves `root` is refused.
pub fn find(root: &MailRoot, mailbox: &Path, id: MessageId) -> Result<Option<EmlxFile>, EmlxError> {
    let entries = match fs::read_dir(mailbox) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(EmlxError::Io {
                path: mailbox.to_owned(),
                source,
            });
        }
    };
    let mut stores = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                return Err(EmlxError::Io {
                    path: mailbox.to_owned(),
                    source,
                });
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_full_uuid(name) {
            stores.push(entry.path());
        }
    }
    stores.sort();

    let dir = partition(id);
    let candidates = [
        (format!("{id}.emlx"), Completeness::Full),
        (format!("{id}.partial.emlx"), Completeness::Partial),
    ];
    for (name, completeness) in candidates {
        for store in &stores {
            let path = store.join(&dir).join(&name);
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => return Err(EmlxError::Io { path, source }),
            };
            if !path.starts_with(root.as_path()) {
                return Err(EmlxError::OutsideRoot(path));
            }
            if !path.is_file() {
                continue;
            }
            return Ok(Some(EmlxFile { path, completeness }));
        }
    }
    Ok(None)
}

/// Reads and parses a message file.
pub fn read(file: &EmlxFile) -> Result<Content, EmlxError> {
    let bytes = match fs::read(&file.path) {
        Ok(bytes) => bytes,
        Err(source) => {
            return Err(EmlxError::Io {
                path: file.path.clone(),
                source,
            });
        }
    };
    parse(&bytes)
}

/// Parses the bytes of an `.emlx` file: a length line, the RFC 822 message, then Mail's plist.
pub fn parse(bytes: &[u8]) -> Result<Content, EmlxError> {
    let message = message_bytes(bytes)?;
    let Some(message) = MessageParser::default().parse(message) else {
        return Ok(Content {
            body: String::new(),
            body_truncated: false,
            attachments: Vec::new(),
        });
    };

    let body_part = body_part(&message);
    let body = match body_part.and_then(|index| part(&message, index)) {
        Some(MessagePart {
            body: PartType::Text(text),
            ..
        }) => text.to_string(),
        Some(MessagePart {
            body: PartType::Html(html),
            ..
        }) => html_to_text(html),
        Some(_) | None => String::new(),
    };
    let (body, body_truncated) = cap(body);

    let mut attachments = Vec::new();
    for index in &message.attachments {
        if Some(*index) == body_part {
            continue;
        }
        let Some(part) = part(&message, *index) else {
            continue;
        };
        attachments.push(Attachment {
            name: part.attachment_name().map(str::to_owned),
            content_type: content_type(part),
            size: part.len(),
        });
    }

    Ok(Content {
        body,
        body_truncated,
        attachments,
    })
}

fn body_part(message: &mail_parser::Message<'_>) -> Option<MessagePartId> {
    for index in &message.text_body {
        if let Some(MessagePart {
            body: PartType::Text(_),
            ..
        }) = part(message, *index)
        {
            return Some(*index);
        }
    }
    for index in &message.html_body {
        if let Some(MessagePart {
            body: PartType::Html(_),
            ..
        }) = part(message, *index)
        {
            return Some(*index);
        }
    }
    for index in &message.attachments {
        let Some(part) = part(message, *index) else {
            continue;
        };
        if part.attachment_name().is_some() {
            continue;
        }
        match part.body {
            PartType::Text(_) | PartType::Html(_) => return Some(*index),
            PartType::Binary(_)
            | PartType::InlineBinary(_)
            | PartType::Message(_)
            | PartType::Multipart(_) => {}
        }
    }
    None
}

fn part<'m>(
    message: &'m mail_parser::Message<'m>,
    index: MessagePartId,
) -> Option<&'m MessagePart<'m>> {
    message.parts.get(usize::try_from(index).ok()?)
}

fn message_bytes(bytes: &[u8]) -> Result<&[u8], EmlxError> {
    let mut newline = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            newline = Some(index);
            break;
        }
    }
    let Some(newline) = newline else {
        return Err(EmlxError::LengthLine);
    };
    let Ok(line) = std::str::from_utf8(&bytes[..newline]) else {
        return Err(EmlxError::LengthLine);
    };
    let line = line.trim();
    if !is_decimal(line) {
        return Err(EmlxError::LengthLine);
    }
    let Ok(length) = line.parse::<usize>() else {
        return Err(EmlxError::LengthLine);
    };
    let rest = &bytes[newline + 1..];
    Ok(&rest[..length.min(rest.len())])
}

fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), HTML_WIDTH).unwrap_or_default()
}

fn cap(body: String) -> (String, bool) {
    let Some((end, _)) = body.char_indices().nth(BODY_CAP) else {
        return (body, false);
    };
    let mut body = body;
    body.truncate(end);
    (body, true)
}

fn content_type(part: &MessagePart<'_>) -> String {
    let Some(content_type) = part.content_type() else {
        return DEFAULT_CONTENT_TYPE.to_owned();
    };
    let main = content_type.c_type.to_ascii_lowercase();
    match &content_type.c_subtype {
        Some(subtype) => format!("{main}/{}", subtype.to_ascii_lowercase()),
        None => main,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::fixture;

    fn id(value: i64) -> MessageId {
        MessageId::new(value).unwrap()
    }

    #[test]
    fn partition_reverses_the_thousands() {
        assert_eq!(partition(id(830)), PathBuf::from("Data/Messages"));
        assert_eq!(partition(id(12_345)), PathBuf::from("Data/2/1/Messages"));
        assert_eq!(partition(id(383_621)), PathBuf::from("Data/3/8/3/Messages"));
        assert_eq!(partition(id(1_000)), PathBuf::from("Data/1/Messages"));
        assert_eq!(
            partition(id(4_102_999)),
            PathBuf::from("Data/2/0/1/4/Messages")
        );
    }

    #[test]
    fn mailbox_dir_suffixes_every_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = MailRoot::new(dir.path()).unwrap();
        let account = AccountId::parse(fixture::GMAIL).unwrap();
        let mailbox = MailboxPath::decode("%5BGmail%5D/All%20Mail").unwrap();
        assert_eq!(
            root.mailbox_dir(&account, &mailbox),
            root.as_path()
                .join(fixture::GMAIL)
                .join("[Gmail].mbox")
                .join("All Mail.mbox")
        );
    }

    #[test]
    fn find_prefers_the_full_file_and_falls_back_to_partial() {
        let fixture = fixture::Fixture::empty();
        let root = MailRoot::new(&fixture.root).unwrap();
        let mailbox = fixture.root.join(fixture::MAIN).join("Inbox.mbox");
        fixture.write_emlx(
            fixture::inbox(12_345, Completeness::Partial),
            "Subject: a\n\nb",
        );
        let found = find(&root, &mailbox, id(12_345)).unwrap().unwrap();
        assert_eq!(found.completeness, Completeness::Partial);
        assert!(found.path.ends_with("Data/2/1/Messages/12345.partial.emlx"));

        fixture.write_emlx(
            fixture::inbox(12_345, Completeness::Full),
            "Subject: a\n\nb",
        );
        let found = find(&root, &mailbox, id(12_345)).unwrap().unwrap();
        assert_eq!(found.completeness, Completeness::Full);
        assert!(found.path.ends_with("Data/2/1/Messages/12345.emlx"));
    }

    #[test]
    fn find_prefers_a_full_file_in_any_store_and_skips_directories() {
        let fixture = fixture::Fixture::empty();
        let root = MailRoot::new(&fixture.root).unwrap();
        let mailbox = fixture.root.join(fixture::MAIN).join("Inbox.mbox");
        let first = mailbox
            .join("0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D")
            .join(partition(id(6)));
        let second = mailbox.join(fixture::STORE).join(partition(id(6)));
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("6.partial.emlx"), "1\na").unwrap();
        fs::write(second.join("6.emlx"), "1\na").unwrap();
        let found = find(&root, &mailbox, id(6)).unwrap().unwrap();
        assert_eq!(found.completeness, Completeness::Full);
        assert!(found.path.starts_with(second.canonicalize().unwrap()));

        fs::create_dir_all(first.join("7.emlx")).unwrap();
        assert_eq!(find(&root, &mailbox, id(7)).unwrap(), None);
    }

    #[test]
    fn find_reports_a_missing_file_or_mailbox_as_none() {
        let fixture = fixture::Fixture::empty();
        let root = MailRoot::new(&fixture.root).unwrap();
        let mailbox = fixture.root.join(fixture::MAIN).join("Inbox.mbox");
        assert_eq!(find(&root, &mailbox, id(7)).unwrap(), None);
        fixture.write_emlx(fixture::inbox(8, Completeness::Full), "Subject: a\n\nb");
        assert_eq!(find(&root, &mailbox, id(7)).unwrap(), None);
    }

    #[test]
    fn find_ignores_directories_that_are_not_store_uuids() {
        let fixture = fixture::Fixture::empty();
        let root = MailRoot::new(&fixture.root).unwrap();
        let mailbox = fixture.root.join(fixture::MAIN).join("Inbox.mbox");
        let stray = mailbox.join("Attachments").join(partition(id(9)));
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join("9.emlx"), "1\na").unwrap();
        assert_eq!(find(&root, &mailbox, id(9)).unwrap(), None);
    }

    #[test]
    fn find_refuses_a_file_outside_the_root() {
        let fixture = fixture::Fixture::empty();
        let outside = tempfile::tempdir().unwrap();
        let store = outside.path().join(fixture::STORE).join(partition(id(5)));
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("5.emlx"), "1\na").unwrap();
        let account = fixture.root.join(fixture::MAIN);
        fs::create_dir_all(&account).unwrap();
        std::os::unix::fs::symlink(outside.path(), account.join("Inbox.mbox")).unwrap();

        let root = MailRoot::new(&fixture.root).unwrap();
        let result = find(&root, &account.join("Inbox.mbox"), id(5));
        let Err(EmlxError::OutsideRoot(_)) = result else {
            panic!("{result:?}");
        };
    }

    #[test]
    fn parse_reads_exactly_the_length_line_bytes() {
        let message = "Subject: hi\r\n\r\nhello";
        let bytes = format!(
            "{}   \n{message}<?xml version=\"1.0\"?><plist/>",
            message.len()
        );
        let content = parse(bytes.as_bytes()).unwrap();
        assert_eq!(content.body, "hello");
        assert!(!content.body_truncated);
        assert!(content.attachments.is_empty());
    }

    #[test]
    fn parse_takes_what_there_is_when_the_length_overshoots() {
        let content = parse(b"9999\nSubject: hi\r\n\r\nhello").unwrap();
        assert_eq!(content.body, "hello");
    }

    #[test]
    fn parse_refuses_a_bad_length_line() {
        for bytes in [
            &b""[..],
            b"no newline",
            b"abc\nSubject: a\n\nb",
            b"\nSubject: a",
            b"-5\nx",
        ] {
            let result = parse(bytes);
            let Err(EmlxError::LengthLine) = result else {
                panic!("{:?}: {result:?}", String::from_utf8_lossy(bytes));
            };
        }
    }

    #[test]
    fn parse_prefers_plain_text_over_html() {
        let message = fixture::alternative_message();
        let content = parse(&fixture::emlx_bytes(&message)).unwrap();
        assert_eq!(content.body.trim(), "Plain version");
    }

    #[test]
    fn parse_converts_html_only_bodies() {
        let message = fixture::html_message();
        let content = parse(&fixture::emlx_bytes(&message)).unwrap();
        assert!(content.body.contains("Big sale"), "{:?}", content.body);
        assert!(content.body.contains("50%"), "{:?}", content.body);
        assert!(!content.body.contains('<'), "{:?}", content.body);
    }

    #[test]
    fn parse_lists_attachments_without_contents() {
        let message = fixture::multipart_message();
        let content = parse(&fixture::emlx_bytes(&message)).unwrap();
        assert_eq!(content.body.trim(), fixture::CYRILLIC_BODY);
        assert_eq!(
            content.attachments,
            vec![Attachment {
                name: Some("report.pdf".to_owned()),
                content_type: "application/pdf".to_owned(),
                size: fixture::ATTACHMENT_BYTES.len(),
            }]
        );
    }

    #[test]
    fn parse_caps_the_body() {
        let body = "я".repeat(BODY_CAP + 10);
        let message =
            format!("Subject: long\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}");
        let content = parse(&fixture::emlx_bytes(&message)).unwrap();
        assert!(content.body_truncated);
        assert_eq!(content.body.chars().count(), BODY_CAP);

        let body = "a".repeat(BODY_CAP);
        let message = format!("Subject: exact\r\n\r\n{body}");
        let content = parse(&fixture::emlx_bytes(&message)).unwrap();
        assert!(!content.body_truncated);
        assert_eq!(content.body.chars().count(), BODY_CAP);
    }

    #[test]
    fn parse_recovers_what_it_can_from_a_partial_message() {
        let content = parse(&fixture::emlx_bytes(fixture::PARTIAL_MESSAGE)).unwrap();
        assert!(
            content.body.contains("The first lines"),
            "{:?}",
            content.body
        );
        assert!(content.attachments.is_empty(), "{:?}", content.attachments);
    }

    #[test]
    fn parse_keeps_unnamed_text_attachments_when_there_is_a_body() {
        let message = "Subject: x\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nBody\r\n--b\r\nContent-Type: text/csv\r\nContent-Disposition: attachment\r\n\r\na,b\r\n--b--\r\n";
        let content = parse(&fixture::emlx_bytes(message)).unwrap();
        assert_eq!(content.body.trim(), "Body");
        assert_eq!(content.attachments.len(), 1);
        assert_eq!(content.attachments[0].name, None);
        assert_eq!(content.attachments[0].content_type, "text/csv");
    }

    #[test]
    fn parse_of_a_message_without_text_has_an_empty_body() {
        let message = "Subject: x\r\nContent-Type: image/png; name=a.png\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=";
        let content = parse(&fixture::emlx_bytes(message)).unwrap();
        assert_eq!(content.body, "");
    }

    #[test]
    fn read_reports_an_unreadable_file() {
        let file = EmlxFile {
            path: PathBuf::from("/nonexistent/1.emlx"),
            completeness: Completeness::Full,
        };
        let result = read(&file);
        let Err(EmlxError::Io { .. }) = result else {
            panic!("{result:?}");
        };
    }
}
