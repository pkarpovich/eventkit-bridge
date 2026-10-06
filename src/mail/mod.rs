pub mod emlx;
pub mod script;
pub mod store;

#[cfg(test)]
pub(crate) mod fixture;

use std::fmt;

use percent_encoding::percent_decode_str;
use serde::Serialize;

use crate::config::is_decimal;

/// A message's `ROWID` in the Envelope Index, which is also its `.emlx` file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MessageId(i64);

impl MessageId {
    /// Accepts a positive `ROWID`.
    pub fn new(value: i64) -> Option<Self> {
        if value <= 0 {
            return None;
        }
        Some(Self(value))
    }

    /// Parses a positive decimal `ROWID` written without sign or leading zero.
    pub fn parse(value: &str) -> Option<Self> {
        if value.starts_with('0') || !is_decimal(value) {
            return None;
        }
        let Ok(value) = value.parse::<i64>() else {
            return None;
        };
        Self::new(value)
    }

    /// Returns the `ROWID`.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A decoded mailbox path such as `[Gmail]/All Mail`, kept as its segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailboxPath(Vec<String>);

/// Why the path of a mailbox URL was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MailboxPathError {
    /// The URL has no path.
    #[error("the mailbox path is empty")]
    Empty,
    /// Two `/` follow each other, or the path ends in `/`.
    #[error("the mailbox path has an empty segment")]
    EmptySegment,
    /// A segment does not decode to UTF-8.
    #[error("a mailbox path segment is not UTF-8")]
    NotUtf8,
    /// A segment is `.` or `..`.
    #[error("a mailbox path segment is `.` or `..`")]
    DotSegment,
    /// A segment decodes to a string holding `/` or NUL.
    #[error("a mailbox path segment holds `/` or NUL")]
    Separator,
}

impl MailboxPath {
    /// Decodes the URL-encoded path of a mailbox URL, given without its leading `/`.
    pub fn decode(encoded: &str) -> Result<Self, MailboxPathError> {
        if encoded.is_empty() {
            return Err(MailboxPathError::Empty);
        }
        let mut segments = Vec::new();
        for segment in encoded.split('/') {
            if segment.is_empty() {
                return Err(MailboxPathError::EmptySegment);
            }
            let Ok(segment) = percent_decode_str(segment).decode_utf8() else {
                return Err(MailboxPathError::NotUtf8);
            };
            if segment == "." || segment == ".." {
                return Err(MailboxPathError::DotSegment);
            }
            if segment.contains('/') || segment.contains('\0') {
                return Err(MailboxPathError::Separator);
            }
            segments.push(segment.into_owned());
        }
        Ok(Self(segments))
    }

    /// The decoded segments, outermost first.
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for MailboxPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_accepts_positive_decimals() {
        assert_eq!(MessageId::parse("383621"), MessageId::new(383621));
        assert_eq!(MessageId::parse("1").map(MessageId::get), Some(1));
        assert_eq!(MessageId::new(42).unwrap().to_string(), "42");
    }

    #[test]
    fn message_id_refuses_everything_else() {
        for value in [
            "",
            "0",
            "012",
            "-5",
            "+5",
            "5a",
            " 5",
            "1.0",
            "99999999999999999999",
        ] {
            assert_eq!(MessageId::parse(value), None, "{value:?}");
        }
        assert_eq!(MessageId::new(0), None);
        assert_eq!(MessageId::new(-1), None);
    }

    #[test]
    fn mailbox_path_decodes_segments() {
        let path = MailboxPath::decode("%5BGmail%5D/All%20Mail").unwrap();
        assert_eq!(path.segments(), ["[Gmail]", "All Mail"]);
        assert_eq!(path.to_string(), "[Gmail]/All Mail");
        let path = MailboxPath::decode("%D0%92%D0%B0%D0%B6%D0%BD%D0%BE%D0%B5").unwrap();
        assert_eq!(path.to_string(), "Важное");
    }

    #[test]
    fn mailbox_path_refuses_crafted_paths() {
        let cases = [
            ("", MailboxPathError::Empty),
            ("Inbox//Sub", MailboxPathError::EmptySegment),
            ("Inbox/", MailboxPathError::EmptySegment),
            ("..", MailboxPathError::DotSegment),
            ("Inbox/../../etc", MailboxPathError::DotSegment),
            ("%2E%2E/x", MailboxPathError::DotSegment),
            ("./Inbox", MailboxPathError::DotSegment),
            ("a%2F..%2Fb", MailboxPathError::Separator),
            ("a%00b", MailboxPathError::Separator),
            ("%FF", MailboxPathError::NotUtf8),
        ];
        for (encoded, error) in cases {
            assert_eq!(MailboxPath::decode(encoded), Err(error), "{encoded:?}");
        }
    }
}
