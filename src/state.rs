use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, Read},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{
    model::{OwnershipProof, SessionId, TmuxSessionId},
    storage::{atomic_write, with_advisory_lock},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Creating,
    #[default]
    Running,
    Stopping,
    Ended,
    Removed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub host: String,
    pub target: String,
    pub directory: String,
    pub preset: Option<String>,
    /// Original shell command, retained privately for an explicit restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Immutable tmux incarnation captured after successful creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_id: Option<TmuxSessionId>,
    /// Private capability shared only with the exact tmux incarnation.
    ///
    /// Legacy records deliberately have no proof and therefore cannot confer
    /// destructive ownership until safely recreated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_proof: Option<OwnershipProof>,
    pub status: SessionStatus,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    pub version: u32,
    pub sessions: Vec<SessionRecord>,
}

impl State {
    pub const CURRENT_VERSION: u32 = 2;

    pub fn validate(&self) -> Result<()> {
        if self.version != Self::CURRENT_VERSION {
            bail!(
                "unsupported state version {}; expected {}",
                self.version,
                Self::CURRENT_VERSION
            );
        }

        let mut session_ids = HashSet::with_capacity(self.sessions.len());
        for (index, session) in self.sessions.iter().enumerate() {
            if !session_ids.insert(session.id) {
                bail!("duplicate session id `{}`", session.id);
            }
            require_nonempty(&session.host, &format!("session at index {index} host"))?;
            require_nonempty(&session.target, &format!("session `{}` target", session.id))?;
            require_nonempty(
                &session.directory,
                &format!("session `{}` directory", session.id),
            )?;
            if let Some(preset) = &session.preset {
                require_nonempty(preset, &format!("session `{}` preset", session.id))?;
            }
            if session.last_used_at < session.created_at {
                bail!(
                    "session `{}` last_used_at must not be earlier than created_at",
                    session.id
                );
            }
            match (session.status, session.closed_at) {
                (
                    SessionStatus::Creating | SessionStatus::Running | SessionStatus::Stopping,
                    Some(_),
                ) => bail!(
                    "non-ended session `{}` must not have closed_at",
                    session.id
                ),
                (SessionStatus::Ended | SessionStatus::Removed, None) => {
                    bail!("ended session `{}` must have closed_at", session.id)
                }
                (_, Some(closed_at)) if closed_at < session.created_at => bail!(
                    "session `{}` closed_at must not be earlier than created_at",
                    session.id
                ),
                _ => {}
            }
            if session.exit_status.is_some()
                && !matches!(session.status, SessionStatus::Ended | SessionStatus::Removed)
            {
                bail!(
                    "non-ended session `{}` must not have exit_status",
                    session.id
                );
            }
            if session.command.as_deref().is_some_and(str::is_empty) {
                bail!("session `{}` command must not be empty", session.id);
            }
        }
        Ok(())
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            sessions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
}

pub struct LockedStateStore<'a> {
    store: &'a StateStore,
}

impl LockedStateStore<'_> {
    pub fn load(&self) -> Result<State> {
        self.store.load_unlocked()
    }

    pub fn save(&self, state: &State) -> Result<()> {
        self.store.save_unlocked(state)
    }
}

impl StateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn update<T>(&self, operation: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        with_advisory_lock(&self.path, || {
            let mut state = self.load_unlocked()?;
            let result = operation(&mut state)?;
            self.save_unlocked(&state)?;
            Ok(result)
        })
    }

    pub fn exclusive<T>(
        &self,
        operation: impl FnOnce(&LockedStateStore<'_>) -> Result<T>,
    ) -> Result<T> {
        with_advisory_lock(&self.path, || operation(&LockedStateStore { store: self }))
    }

    pub fn load(&self) -> Result<State> {
        with_advisory_lock(&self.path, || self.load_unlocked())
    }

    fn load_unlocked(&self) -> Result<State> {
        let source = match read_regular_state_file(&self.path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("read state `{}`", self.path.display()));
            }
        };

        let document: serde_json::Value = serde_json::from_str(&source)
            .with_context(|| format!("parse state `{}` as JSON", self.path.display()))?;
        let version = document
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "state `{}` must contain a non-negative integer `version`",
                    self.path.display()
                )
            })?;

        match version {
            2 => {
                let state: State = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 2 from `{}`", self.path.display())
                })?;
                state.validate()?;
                Ok(state)
            }
            1 => {
                let legacy: StateV1 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 1 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                self.save_unlocked(&state)
                    .with_context(|| format!("rewrite migrated state `{}`", self.path.display()))?;
                Ok(state)
            }
            0 => {
                let legacy: StateV0 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 0 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                self.save_unlocked(&state)
                    .with_context(|| format!("rewrite migrated state `{}`", self.path.display()))?;
                Ok(state)
            }
            other => bail!(
                "unsupported state version {other} in `{}`; supported versions are 0 through {}",
                self.path.display(),
                State::CURRENT_VERSION
            ),
        }
    }

    pub fn save(&self, state: &State) -> Result<()> {
        with_advisory_lock(&self.path, || self.save_unlocked(state))
    }

    fn save_unlocked(&self, state: &State) -> Result<()> {
        state.validate()?;
        let mut serialized =
            serde_json::to_string_pretty(state).context("serialize state as JSON")?;
        serialized.push('\n');
        atomic_write(&self.path, serialized.as_bytes())
            .with_context(|| format!("save state `{}`", self.path.display()))
    }
}

fn read_regular_state_file(path: &std::path::Path) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);

    let mut file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state path is a symbolic link",
            ));
        }
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state path is not a regular file",
        ));
    }

    let mut source = String::new();
    file.read_to_string(&mut source)?;
    Ok(source)
}

fn require_nonempty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

#[derive(Deserialize)]
struct StateV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecordV1>,
}

#[derive(Deserialize)]
struct SessionRecordV1 {
    id: SessionId,
    host: String,
    target: String,
    directory: String,
    preset: Option<String>,
    status: SessionStatusV1,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionStatusV1 {
    Active,
    Closing,
    Closed,
}

impl StateV1 {
    fn migrate(self) -> State {
        State {
            version: State::CURRENT_VERSION,
            sessions: self
                .sessions
                .into_iter()
                .map(|session| {
                    let status = match session.status {
                        SessionStatusV1::Active => SessionStatus::Running,
                        SessionStatusV1::Closing => SessionStatus::Stopping,
                        SessionStatusV1::Closed => SessionStatus::Ended,
                    };
                    SessionRecord {
                        id: session.id,
                        host: session.host,
                        target: session.target,
                        directory: session.directory,
                        preset: session.preset,
                        command: None,
                        tmux_session_id: None,
                        ownership_proof: None,
                        status,
                        created_at: session.created_at,
                        last_used_at: session.last_used_at,
                        closed_at: session.closed_at,
                        exit_status: None,
                    }
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct StateV0 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecordV0>,
}

#[derive(Deserialize)]
struct SessionRecordV0 {
    id: SessionId,
    host: String,
    target: String,
    directory: String,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
}

impl StateV0 {
    fn migrate(self) -> State {
        State {
            version: State::CURRENT_VERSION,
            sessions: self
                .sessions
                .into_iter()
                .map(|session| SessionRecord {
                    id: session.id,
                    host: session.host,
                    target: session.target,
                    directory: session.directory,
                    preset: None,
                    command: None,
                    tmux_session_id: None,
                    ownership_proof: None,
                    status: SessionStatus::Running,
                    created_at: session.created_at,
                    last_used_at: session.last_used_at,
                    closed_at: None,
                    exit_status: None,
                })
                .collect(),
        }
    }
}
