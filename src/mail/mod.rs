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

/// One of Mail's seven flag colours, in the order of its flag menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagColor {
    /// Index 0.
    Red,
    /// Index 1.
    Orange,
    /// Index 2.
    Yellow,
    /// Index 3.
    Green,
    /// Index 4.
    Blue,
    /// Index 5.
    Purple,
    /// Index 6.
    Gray,
}

impl FlagColor {
    /// Every colour, by index.
    pub const ALL: [FlagColor; 7] = [
        FlagColor::Red,
        FlagColor::Orange,
        FlagColor::Yellow,
        FlagColor::Green,
        FlagColor::Blue,
        FlagColor::Purple,
        FlagColor::Gray,
    ];

    /// The colour at Mail's flag index 0 to 6.
    pub fn from_index(index: i64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|color| i64::from(color.index()) == index)
    }

    /// The colour called `name`, in lowercase.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|color| color.name() == name)
    }

    /// Mail's flag index.
    pub fn index(self) -> u8 {
        match self {
            FlagColor::Red => 0,
            FlagColor::Orange => 1,
            FlagColor::Yellow => 2,
            FlagColor::Green => 3,
            FlagColor::Blue => 4,
            FlagColor::Purple => 5,
            FlagColor::Gray => 6,
        }
    }

    /// The lowercase name used in the API.
    pub fn name(self) -> &'static str {
        match self {
            FlagColor::Red => "red",
            FlagColor::Orange => "orange",
            FlagColor::Yellow => "yellow",
            FlagColor::Green => "green",
            FlagColor::Blue => "blue",
            FlagColor::Purple => "purple",
            FlagColor::Gray => "gray",
        }
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
    fn flag_color_maps_index_and_name() {
        for (index, name) in [
            (0, "red"),
            (1, "orange"),
            (2, "yellow"),
            (3, "green"),
            (4, "blue"),
            (5, "purple"),
            (6, "gray"),
        ] {
            let color = FlagColor::from_index(index).unwrap();
            assert_eq!(color.name(), name);
            assert_eq!(i64::from(color.index()), index);
            assert_eq!(FlagColor::from_name(name), Some(color));
            assert_eq!(
                serde_json::to_string(&color).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn flag_color_refuses_everything_else() {
        assert_eq!(FlagColor::from_index(-1), None);
        assert_eq!(FlagColor::from_index(7), None);
        for name in ["", "Red", "pink", " red"] {
            assert_eq!(FlagColor::from_name(name), None, "{name:?}");
        }
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
