//! jj-lib settings built from Reef's own configuration, not from jj's.
//!
//! jj's user config files (`~/.config/jj/config.toml` and friends) are
//! deliberately not read. The daemon will have its own configuration, and a
//! user's jj config must not change how Reef snapshots.

use std::fmt::Write as _;

use jj_lib::config::{ConfigLayer, ConfigLoadError, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

use crate::error::{Classify, ErrorKind};

pub use jj_lib::config::ConfigGetError;

/// Config key holding the largest new file a snapshot starts tracking.
pub const MAX_NEW_FILE_SIZE_KEY: &str = "snapshot.max-new-file-size";

/// jj's default for [`MAX_NEW_FILE_SIZE_KEY`]. jj-cli sets it; jj-lib does not.
pub const DEFAULT_MAX_NEW_FILE_SIZE: &str = "1MiB";

/// The author and committer recorded on commits Reef writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Display name, as in `user.name`.
    pub name: String,
    /// Email address, as in `user.email`.
    pub email: String,
}

/// Failure to build [`UserSettings`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// The generated config layer did not parse.
    #[error("invalid generated jj config")]
    Parse(#[from] ConfigLoadError),
    /// A config value could not be read.
    #[error("invalid jj config value")]
    Get(#[from] ConfigGetError),
    /// A byte size value was not a number with an optional unit.
    #[error("invalid byte size {0:?}; expected a number with an optional B, KiB, MiB, or GiB unit")]
    ByteSize(String),
}

impl Classify for SettingsError {
    fn kind(&self) -> ErrorKind {
        match self {
            // The layer is generated here with escaped values; failing to parse it is a bug.
            Self::Parse(_) => ErrorKind::Internal,
            Self::Get(_) | Self::ByteSize(_) => ErrorKind::InvalidArgument,
        }
    }
}

/// Builds jj-lib settings from jj-lib's defaults plus Reef's identity and
/// snapshot limits.
pub fn user_settings(identity: &Identity) -> Result<UserSettings, SettingsError> {
    let mut config = StackedConfig::with_defaults();
    let layer = ConfigLayer::parse(
        ConfigSource::User,
        &format!(
            "user.name = {}\nuser.email = {}\n{MAX_NEW_FILE_SIZE_KEY} = {}\n",
            toml_string(&identity.name),
            toml_string(&identity.email),
            toml_string(DEFAULT_MAX_NEW_FILE_SIZE),
        ),
    )?;
    config.add_layer(layer);
    Ok(UserSettings::from_config_and_home_dir(config, None)?)
}

/// Reads [`MAX_NEW_FILE_SIZE_KEY`] back from `settings`. Zero means no limit,
/// as in jj.
pub fn max_new_file_size(settings: &UserSettings) -> Result<u64, SettingsError> {
    let text: String = settings.get(MAX_NEW_FILE_SIZE_KEY)?;
    match parse_byte_size(&text)? {
        0 => Ok(u64::MAX),
        size => Ok(size),
    }
}

fn parse_byte_size(text: &str) -> Result<u64, SettingsError> {
    let err = || SettingsError::ByteSize(text.to_owned());
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let number: u64 = number.parse().map_err(|_| err())?;
    let multiplier: u64 = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return Err(err()),
    };
    number.checked_mul(multiplier).ok_or_else(err)
}

/// Quotes `value` as a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_parse() {
        assert_eq!(parse_byte_size("1MiB").unwrap(), 1 << 20);
        assert_eq!(parse_byte_size("512 KiB").unwrap(), 512 << 10);
        assert_eq!(parse_byte_size("42").unwrap(), 42);
        assert!(parse_byte_size("1MB").is_err());
        assert!(parse_byte_size("MiB").is_err());
    }

    #[test]
    fn identity_with_quotes_round_trips() {
        let settings = user_settings(&Identity {
            name: "A \"quoted\" \\ name".into(),
            email: "a@example.com".into(),
        })
        .unwrap();
        assert_eq!(settings.user_name(), "A \"quoted\" \\ name");
        assert_eq!(max_new_file_size(&settings).unwrap(), 1 << 20);
    }
}
