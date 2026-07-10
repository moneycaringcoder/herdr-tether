use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

/// Where Herdr should place a newly opened terminal.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    #[default]
    SplitRight,
    SplitDown,
    NewTab,
}

/// A stable, Tether-owned identifier for a durable session.
///
/// Its textual representation is also safe to use as a tmux session name.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tether-{}", self.0.simple())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error(
    "invalid session id `{value}`: expected `tether-` followed by a lowercase UUIDv7 in 32 hexadecimal digits"
)]
pub struct SessionIdError {
    value: String,
}

impl FromStr for SessionId {
    type Err = SessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || SessionIdError {
            value: value.to_owned(),
        };
        let Some(encoded) = value.strip_prefix("tether-") else {
            return Err(invalid());
        };
        if encoded.len() != 32
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid());
        }

        let uuid = Uuid::parse_str(encoded).map_err(|_| invalid())?;
        if uuid.get_version_num() != 7 {
            return Err(invalid());
        }

        Ok(Self(uuid))
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}
