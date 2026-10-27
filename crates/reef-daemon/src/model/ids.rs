//! Identifiers of the daemon's own records: repositories, workspaces, and
//! leases.
//!
//! Each id is 64 random bits, written as 16 lowercase hex characters. Parsing
//! accepts uppercase and normalizes it, so an id pasted in either case names
//! the same record. Prefix matching, as the CLI offers it, is a query helper
//! in [`crate::db`], not part of these types.

use std::fmt;
use std::str::FromStr;

use rand::RngExt as _;
use reef_lib::error::{Classify, ErrorKind};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Text that is not a whole id: anything but exactly 16 hex characters.
#[derive(Debug, thiserror::Error)]
#[error("invalid id {0:?}: expected 16 hex characters")]
pub struct ParseIdError(pub String);

impl Classify for ParseIdError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// Number of hex characters in an id's text form.
pub const ID_HEX_LEN: usize = 16;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        ///
        /// 64 random bits, displayed as 16 lowercase hex characters and
        /// stored in SQLite as that text.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 8]);

        impl $name {
            /// A new random id from the thread-local generator.
            pub fn generate() -> Self {
                Self(rand::rng().random())
            }

            /// The id as 16 lowercase hex characters, the same text as
            /// [`Display`](fmt::Display).
            pub fn as_hex(&self) -> String {
                self.to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            /// Parses exactly 16 hex characters, in either case.
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                parse_hex(text).map(Self)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.to_string()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value
                    .as_str()?
                    .parse()
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

id_type!(RepoId, "A registered repository.");
id_type!(WorkspaceId, "A leased workspace.");
id_type!(LeaseId, "One lease on a workspace.");
id_type!(PresenceId, "One presence entry (R2.10).");

impl WorkspaceId {
    /// The name of the jj workspace backing this Reef workspace,
    /// `reef-<id>` (ADR 0014).
    pub fn jj_workspace_name(&self) -> String {
        format!("reef-{self}")
    }
}

fn parse_hex(text: &str) -> Result<[u8; 8], ParseIdError> {
    let invalid = || ParseIdError(text.to_owned());
    let digits = text.as_bytes();
    if digits.len() != ID_HEX_LEN {
        return Err(invalid());
    }
    let mut bytes = [0; 8];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, &[high, low]) in bytes.iter_mut().zip(pairs) {
        let high = hex_value(high).ok_or_else(invalid)?;
        let low = hex_value(low).ok_or_else(invalid)?;
        *byte = (high << 4) | low;
    }
    Ok(bytes)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    const HEX: &str = "0123456789abcdef";

    #[test]
    fn display_and_from_str_round_trip() {
        let id: WorkspaceId = HEX.parse().unwrap();
        assert_eq!(id.to_string(), HEX);
        assert_eq!(id.as_hex(), HEX);
        assert_eq!(format!("{id:?}"), "WorkspaceId(0123456789abcdef)");

        let generated = RepoId::generate();
        assert_eq!(generated.to_string().parse::<RepoId>().unwrap(), generated);
        assert_ne!(
            RepoId::generate(),
            generated,
            "64 random bits do not repeat"
        );
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        for text in [
            "0123456789abcde",
            "0123456789abcdef0",
            "",
            "0123456789abcdeg",
            "reef-0123456789ab",
        ] {
            let error = text.parse::<LeaseId>().unwrap_err();
            assert_eq!(error.0, text);
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        }
    }

    #[test]
    fn accepts_uppercase_and_stores_lowercase() {
        let upper: RepoId = "0123456789ABCDEF".parse().unwrap();
        let lower: RepoId = HEX.parse().unwrap();
        assert_eq!(upper, lower);
        assert_eq!(upper.to_string(), HEX);
    }

    #[test]
    fn serde_uses_the_hex_string() {
        let id: LeaseId = HEX.parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{HEX}\""));
        assert_eq!(serde_json::from_str::<LeaseId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<LeaseId>("\"0123\"").is_err());
        assert!(serde_json::from_str::<LeaseId>("17").is_err());
    }

    #[test]
    fn sql_stores_lowercase_text() {
        let conn = Connection::open_in_memory().unwrap();
        let id: RepoId = "0123456789ABCDEF".parse().unwrap();
        let (text, back): (String, RepoId) = conn
            .query_row("SELECT ?1, ?1", [id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!(text, HEX);
        assert_eq!(back, id);
        let bad = conn.query_row("SELECT 'xyz'", [], |row| row.get::<_, RepoId>(0));
        assert!(bad.is_err());
    }

    #[test]
    fn jj_workspace_name_prefixes_reef() {
        let id: WorkspaceId = HEX.parse().unwrap();
        assert_eq!(id.jj_workspace_name(), "reef-0123456789abcdef");
    }
}
