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
    ReplaceCurrentPane,
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

/// A private, unguessable capability proving that a tmux incarnation was
/// created for one persisted Tether reservation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OwnershipProof(Uuid);

impl OwnershipProof {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for OwnershipProof {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OwnershipProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.simple().to_string())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid ownership proof")]
pub struct OwnershipProofError;

impl FromStr for OwnershipProof {
    type Err = OwnershipProofError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(OwnershipProofError);
        }
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| OwnershipProofError)
    }
}

/// The immutable tmux server identity captured when an owned session is created.
///
/// Destructive operations target this identity, never a reusable session name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TmuxSessionId(u64);

impl fmt::Display for TmuxSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "${}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid tmux session identity `{value}`")]
pub struct TmuxSessionIdError {
    value: String,
}

impl FromStr for TmuxSessionId {
    type Err = TmuxSessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || TmuxSessionIdError {
            value: value.to_owned(),
        };
        let encoded = value.strip_prefix('$').ok_or_else(invalid)?;
        if encoded.is_empty() || !encoded.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        encoded.parse().map(Self).map_err(|_| invalid())
    }
}

/// A validated, non-Tether tmux session name that may only be attached.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExternalSessionName(String);

impl ExternalSessionName {
    pub const MAX_BYTES: usize = 256;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalSessionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unsafe external tmux session name")]
pub struct ExternalSessionNameError;

impl FromStr for ExternalSessionName {
    type Err = ExternalSessionNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || value.starts_with("tether-")
            || value.starts_with('$')
            || !value
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
            || value.bytes().any(|byte| matches!(byte, b':' | b'.'))
        {
            return Err(ExternalSessionNameError);
        }
        Ok(Self(value.to_owned()))
    }
}

/// A harness-neutral identifier for a persisted orchestration group.
///
/// The restricted representation is safe for logs, terminal labels, and exact
/// state lookups without carrying host, path, or command data.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OrchestrationGroupId(String);

impl OrchestrationGroupId {
    pub const MAX_BYTES: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrchestrationGroupId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid orchestration group id")]
pub struct OrchestrationGroupIdError;

impl FromStr for OrchestrationGroupId {
    type Err = OrchestrationGroupIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > Self::MAX_BYTES
            || !bytes[0].is_ascii_lowercase()
            || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
            || !bytes.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
            || bytes
                .windows(2)
                .any(|pair| matches!(pair[0], b'-' | b'_') && matches!(pair[1], b'-' | b'_'))
        {
            return Err(OrchestrationGroupIdError);
        }
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for OrchestrationGroupId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OrchestrationGroupId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A persisted opaque epoch for one exact orchestration membership.
///
/// Removing and re-adding the same session always creates a new value so
/// asynchronous observation from an earlier authorization cannot cross epochs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OrchestrationMembershipId(Uuid);

impl OrchestrationMembershipId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for OrchestrationMembershipId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OrchestrationMembershipId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.simple().to_string())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid orchestration membership id")]
pub struct OrchestrationMembershipIdError;

impl FromStr for OrchestrationMembershipId {
    type Err = OrchestrationMembershipIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(OrchestrationMembershipIdError);
        }
        let uuid = Uuid::parse_str(value).map_err(|_| OrchestrationMembershipIdError)?;
        if uuid.get_version_num() != 7 {
            return Err(OrchestrationMembershipIdError);
        }
        Ok(Self(uuid))
    }
}

impl Serialize for OrchestrationMembershipId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OrchestrationMembershipId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A bounded, display-safe label for an orchestration group or member.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OrchestrationTitle(String);

impl OrchestrationTitle {
    pub const MAX_BYTES: usize = 128;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrchestrationTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unsafe orchestration title")]
pub struct OrchestrationTitleError;

impl FromStr for OrchestrationTitle {
    type Err = OrchestrationTitleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || value.trim() != value
            || !value
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
        {
            return Err(OrchestrationTitleError);
        }
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for OrchestrationTitle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OrchestrationTitle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
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
