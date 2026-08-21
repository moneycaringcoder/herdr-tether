use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, Read},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{
    model::{
        HerdrAgentKind, OrchestrationGroupId, OrchestrationMembershipId, OrchestrationTitle,
        OwnershipProof, SessionId, TmuxSessionId,
    },
    storage::{atomic_write_resolved, with_advisory_lock},
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

pub(crate) fn is_normal_session(record: &SessionRecord) -> bool {
    record.status != SessionStatus::Removed
}

pub(crate) fn compare_normal_sessions<T: Ord>(
    left_status: SessionStatus,
    left_last_used_at: chrono::DateTime<chrono::Utc>,
    left_tie_breaker: T,
    right_status: SessionStatus,
    right_last_used_at: chrono::DateTime<chrono::Utc>,
    right_tie_breaker: T,
) -> std::cmp::Ordering {
    normal_status_group(left_status)
        .cmp(&normal_status_group(right_status))
        .then_with(|| right_last_used_at.cmp(&left_last_used_at))
        .then_with(|| left_tie_breaker.cmp(&right_tie_breaker))
}

fn normal_status_group(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::Creating | SessionStatus::Running | SessionStatus::Stopping => 0,
        SessionStatus::Ended => 1,
        SessionStatus::Removed => 2,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub id: SessionId,
    pub host: String,
    pub target: String,
    pub directory: String,
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_agent: Option<HerdrAgentKind>,
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

/// How long after starting a failing end still counts as immediate.
///
/// A command that fails this fast failed on its own terms rather than on the
/// work it was given, so restarting it unchanged reproduces the failure.
pub const FAST_FAILURE_WINDOW: TimeDelta = TimeDelta::seconds(10);
/// How long a restart is paced after an immediate failure.
pub const RESTART_PACE: TimeDelta = TimeDelta::seconds(30);

impl SessionRecord {
    /// Whether this workload's command failed as soon as it started.
    pub fn failed_immediately(&self) -> bool {
        self.paced_restart_until().is_some()
    }

    /// When a restart of this workload stops being paced, if it is paced.
    ///
    /// The evidence is already recorded: `last_used_at` is stamped when the
    /// workload starts running and `closed_at` when it ends, so the difference
    /// is how long the last incarnation lasted. Nothing is counted across
    /// restarts, because a restart deliberately keeps no history of the
    /// incarnation it replaced — a paced restart reports one failure that
    /// arrived at once, which is the shape of a loop, rather than a tally.
    ///
    /// An end whose status `tmux` could not report is not paced: an unknown
    /// outcome is not a failure.
    pub fn paced_restart_until(&self) -> Option<DateTime<Utc>> {
        if self.status != SessionStatus::Ended
            || !self.exit_status.is_some_and(|status| status != 0)
        {
            return None;
        }
        let closed_at = self.closed_at?;
        let ran_for = closed_at.signed_duration_since(self.last_used_at);
        (ran_for <= FAST_FAILURE_WINDOW).then(|| closed_at + RESTART_PACE)
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;

    fn ended(ran_for: TimeDelta, exit_status: Option<i32>) -> SessionRecord {
        let started = DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        SessionRecord {
            herdr_agent: None,
            id: "tether-0197f198000070008000000000000001".parse().unwrap(),
            host: "local".to_owned(),
            target: "local".to_owned(),
            directory: "/srv/app".to_owned(),
            preset: None,
            command: Some("exec shell".to_owned()),
            tmux_session_id: None,
            ownership_proof: None,
            status: SessionStatus::Ended,
            created_at: started,
            last_used_at: started,
            closed_at: Some(started + ran_for),
            exit_status,
        }
    }

    #[test]
    fn a_failure_that_arrives_at_once_paces_the_next_restart() {
        let record = ended(TimeDelta::milliseconds(400), Some(1));
        assert!(record.failed_immediately());
        assert_eq!(
            record.paced_restart_until(),
            record.closed_at.map(|closed_at| closed_at + RESTART_PACE)
        );
    }

    #[test]
    fn work_that_ran_before_failing_is_not_paced() {
        // A command that did its job for a while and then failed is a different
        // event: restarting it is reasonable, and pacing it would be in the way.
        let record = ended(FAST_FAILURE_WINDOW + TimeDelta::seconds(1), Some(1));
        assert!(!record.failed_immediately());
        assert_eq!(record.paced_restart_until(), None);
    }

    #[test]
    fn a_clean_or_unknown_end_is_never_paced() {
        assert!(!ended(TimeDelta::milliseconds(400), Some(0)).failed_immediately());
        // An explicit Stop records no status, and `tmux` cannot always report
        // one. Neither is evidence of a failure to pace.
        assert!(!ended(TimeDelta::milliseconds(400), None).failed_immediately());
    }

    #[test]
    fn a_workload_that_has_not_ended_is_never_paced() {
        for status in [
            SessionStatus::Creating,
            SessionStatus::Running,
            SessionStatus::Stopping,
            SessionStatus::Removed,
        ] {
            let mut record = ended(TimeDelta::milliseconds(400), Some(1));
            record.status = status;
            assert!(!record.failed_immediately(), "{status:?}");
        }
    }

    #[test]
    fn the_pace_is_measured_from_the_end_rather_than_from_now() {
        // The window has to survive Tether restarting, so it is anchored to the
        // recorded end instead of to when someone happened to look.
        let record = ended(TimeDelta::milliseconds(400), Some(1));
        let until = record.paced_restart_until().unwrap();
        assert_eq!(
            until.signed_duration_since(record.closed_at.unwrap()),
            RESTART_PACE
        );
    }
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationCapabilities {
    pub observe_output: bool,
    pub open_interactive: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompt_agent: bool,
}

impl OrchestrationCapabilities {
    pub fn is_empty(self) -> bool {
        !self.observe_output && !self.open_interactive && !self.prompt_agent
    }
}
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationMember {
    pub session_id: SessionId,
    pub membership_id: OrchestrationMembershipId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<OrchestrationTitle>,
    pub capabilities: OrchestrationCapabilities,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationGroup {
    pub id: OrchestrationGroupId,
    pub title: OrchestrationTitle,
    pub orchestrator_session_id: SessionId,
    pub workers: Vec<OrchestrationMember>,
}

impl OrchestrationGroup {
    pub const MAX_WORKERS: usize = 64;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub sessions: Vec<SessionRecord>,
    pub orchestration_groups: Vec<OrchestrationGroup>,
}

impl State {
    pub const CURRENT_VERSION: u32 = 4;
    pub const MAX_ORCHESTRATION_GROUPS: usize = 32;
    pub const MAX_SESSIONS: usize = 1_024;
    pub const MAX_STRING_BYTES: usize = 16 * 1024;
    pub const MAX_COMMAND_BYTES: usize = 256 * 1024;

    pub fn validate(&self) -> Result<()> {
        if self.version != Self::CURRENT_VERSION {
            bail!(
                "unsupported state version {}; expected {}",
                self.version,
                Self::CURRENT_VERSION
            );
        }
        if self.sessions.len() > Self::MAX_SESSIONS {
            bail!(
                "state sessions may contain at most {} entries",
                Self::MAX_SESSIONS
            );
        }
        if self.orchestration_groups.len() > Self::MAX_ORCHESTRATION_GROUPS {
            bail!(
                "state orchestration groups may contain at most {} entries",
                Self::MAX_ORCHESTRATION_GROUPS
            );
        }

        let mut session_ids = HashSet::with_capacity(self.sessions.len());
        for (index, session) in self.sessions.iter().enumerate() {
            if !session_ids.insert(session.id) {
                bail!("duplicate session id `{}`", session.id);
            }
            require_nonempty(&session.host, &format!("session at index {index} host"))?;
            require_max_bytes(&session.host, Self::MAX_STRING_BYTES, "session host")?;
            require_nonempty(&session.target, &format!("session `{}` target", session.id))?;
            require_nonempty(
                &session.directory,
                &format!("session `{}` directory", session.id),
            )?;
            require_max_bytes(&session.target, Self::MAX_STRING_BYTES, "session target")?;
            require_max_bytes(
                &session.directory,
                Self::MAX_STRING_BYTES,
                "session directory",
            )?;
            if let Some(preset) = &session.preset {
                require_nonempty(preset, &format!("session `{}` preset", session.id))?;
                require_max_bytes(preset, Self::MAX_STRING_BYTES, "session preset")?;
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
                ) => bail!("non-ended session `{}` must not have closed_at", session.id),
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
                && !matches!(
                    session.status,
                    SessionStatus::Ended | SessionStatus::Removed
                )
            {
                bail!(
                    "non-ended session `{}` must not have exit_status",
                    session.id
                );
            }
            if session.command.as_deref().is_some_and(str::is_empty) {
                bail!("session `{}` command must not be empty", session.id);
            }
            if let Some(command) = &session.command {
                require_max_bytes(command, Self::MAX_COMMAND_BYTES, "session command")?;
            }
        }
        let mut group_ids = HashSet::with_capacity(self.orchestration_groups.len());
        let mut membership_ids = HashSet::new();
        for group in &self.orchestration_groups {
            if !group_ids.insert(&group.id) {
                bail!("duplicate orchestration group id `{}`", group.id);
            }
            if group.workers.len() > OrchestrationGroup::MAX_WORKERS {
                bail!(
                    "orchestration group workers may contain at most {} entries",
                    OrchestrationGroup::MAX_WORKERS
                );
            }
            let mut worker_ids = HashSet::with_capacity(group.workers.len());
            for worker in &group.workers {
                if worker.session_id == group.orchestrator_session_id {
                    bail!(
                        "orchestration group `{}` orchestrator must not also be a worker",
                        group.id
                    );
                }
                if !worker_ids.insert(worker.session_id) {
                    bail!(
                        "duplicate worker session `{}` in orchestration group `{}`",
                        worker.session_id,
                        group.id
                    );
                }
                if !membership_ids.insert(worker.membership_id) {
                    bail!(
                        "duplicate orchestration membership id `{}`",
                        worker.membership_id
                    );
                }
                if worker.capabilities.is_empty() {
                    bail!(
                        "worker session `{}` in orchestration group `{}` must declare a capability",
                        worker.session_id,
                        group.id
                    );
                }
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
            orchestration_groups: Vec::new(),
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

    pub const MAX_PERSISTED_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_INPUT_BYTES: usize = Self::MAX_PERSISTED_BYTES;

    pub fn update<T>(&self, operation: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        with_advisory_lock(&self.path, |path| {
            let store = Self::new(path.to_owned());
            let mut state = store.load_unlocked()?;
            let result = operation(&mut state)?;
            store.save_unlocked(&state)?;
            Ok(result)
        })
    }

    pub fn exclusive<T>(
        &self,
        operation: impl FnOnce(&LockedStateStore<'_>) -> Result<T>,
    ) -> Result<T> {
        with_advisory_lock(&self.path, |path| {
            let store = Self::new(path.to_owned());
            operation(&LockedStateStore { store: &store })
        })
    }

    pub fn load(&self) -> Result<State> {
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).load_unlocked()
        })
    }

    /// Loads and validates state while migrating legacy schemas only in memory.
    ///
    /// Observation commands use this path so reading stable state never rewrites
    /// bytes. Explicit mutating/load migration paths continue to persist upgrades.
    pub fn load_read_only(&self) -> Result<State> {
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).load_unlocked_with_migration(false)
        })
    }

    fn load_unlocked(&self) -> Result<State> {
        self.load_unlocked_with_migration(true)
    }

    fn load_unlocked_with_migration(&self, persist_migration: bool) -> Result<State> {
        let source = match read_regular_state_file(&self.path, Self::MAX_INPUT_BYTES) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Err(error.into()),
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
            4 => {
                let state: State = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 4 from `{}`", self.path.display())
                })?;
                state.validate()?;
                Ok(state)
            }
            3 => {
                let legacy: StateV3 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 3 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                if persist_migration {
                    self.save_unlocked(&state).with_context(|| {
                        format!("rewrite migrated state `{}`", self.path.display())
                    })?;
                }
                Ok(state)
            }
            2 => {
                let legacy: StateV2 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 2 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                if persist_migration {
                    self.save_unlocked(&state).with_context(|| {
                        format!("rewrite migrated state `{}`", self.path.display())
                    })?;
                }
                Ok(state)
            }
            1 => {
                let legacy: StateV1 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 1 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                if persist_migration {
                    self.save_unlocked(&state).with_context(|| {
                        format!("rewrite migrated state `{}`", self.path.display())
                    })?;
                }
                Ok(state)
            }
            0 => {
                let legacy: StateV0 = serde_json::from_str(&source).with_context(|| {
                    format!("decode state version 0 from `{}`", self.path.display())
                })?;
                let state = legacy.migrate();
                state.validate()?;
                if persist_migration {
                    self.save_unlocked(&state).with_context(|| {
                        format!("rewrite migrated state `{}`", self.path.display())
                    })?;
                }
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
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).save_unlocked(state)
        })
    }

    fn save_unlocked(&self, state: &State) -> Result<()> {
        state.validate()?;
        let mut serialized =
            serde_json::to_string_pretty(state).context("serialize state as JSON")?;
        serialized.push('\n');
        require_serialized_state_size(&serialized)?;
        atomic_write_resolved(&self.path, serialized.as_bytes())
            .with_context(|| format!("save state `{}`", self.path.display()))
    }
}

fn require_serialized_state_size(serialized: &str) -> Result<()> {
    if serialized.len() > StateStore::MAX_PERSISTED_BYTES {
        bail!(
            "serialized state may contain at most {} bytes",
            StateStore::MAX_PERSISTED_BYTES
        );
    }
    Ok(())
}

fn read_regular_state_file(path: &std::path::Path, max_bytes: usize) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW);

    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state path is not a regular file",
        ));
    }
    let size = file.metadata()?.len();
    if size > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("state input may contain at most {max_bytes} bytes"),
        ));
    }

    let mut source = String::with_capacity(size as usize);
    file.take(max_bytes as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("state input may contain at most {max_bytes} bytes"),
        ));
    }
    Ok(source)
}

fn require_nonempty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn require_max_bytes(value: &str, max_bytes: usize, field: &str) -> Result<()> {
    if value.len() > max_bytes {
        bail!("{field} may contain at most {max_bytes} bytes");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateV3 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    orchestration_groups: Vec<OrchestrationGroupV3>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationGroupV3 {
    id: OrchestrationGroupId,
    title: OrchestrationTitle,
    orchestrator_session_id: SessionId,
    workers: Vec<OrchestrationMemberV3>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationMemberV3 {
    session_id: SessionId,
    #[serde(default)]
    title: Option<OrchestrationTitle>,
    capabilities: OrchestrationCapabilities,
}

impl StateV3 {
    fn migrate(self) -> State {
        State {
            version: State::CURRENT_VERSION,
            sessions: self.sessions,
            orchestration_groups: self
                .orchestration_groups
                .into_iter()
                .map(|group| OrchestrationGroup {
                    id: group.id,
                    title: group.title,
                    orchestrator_session_id: group.orchestrator_session_id,
                    workers: group
                        .workers
                        .into_iter()
                        .map(|worker| OrchestrationMember {
                            session_id: worker.session_id,
                            membership_id: OrchestrationMembershipId::new(),
                            title: worker.title,
                            capabilities: worker.capabilities,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateV2 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecordV2>,
}

/// Frozen v0.3.0 session shape used only for migration from stable schema v2.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecordV2 {
    id: SessionId,
    host: String,
    target: String,
    directory: String,
    preset: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    tmux_session_id: Option<TmuxSessionId>,
    #[serde(default)]
    ownership_proof: Option<OwnershipProof>,
    status: SessionStatus,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    exit_status: Option<i32>,
}

impl From<SessionRecordV2> for SessionRecord {
    fn from(record: SessionRecordV2) -> Self {
        Self {
            id: record.id,
            host: record.host,
            target: record.target,
            directory: record.directory,
            preset: record.preset,
            herdr_agent: None,
            command: record.command,
            tmux_session_id: record.tmux_session_id,
            ownership_proof: record.ownership_proof,
            status: record.status,
            created_at: record.created_at,
            last_used_at: record.last_used_at,
            closed_at: record.closed_at,
            exit_status: record.exit_status,
        }
    }
}

impl StateV2 {
    fn migrate(self) -> State {
        State {
            version: State::CURRENT_VERSION,
            sessions: self.sessions.into_iter().map(Into::into).collect(),
            orchestration_groups: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecordV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
                        herdr_agent: None,
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
            orchestration_groups: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateV0 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionRecordV0>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
                    herdr_agent: None,
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
            orchestration_groups: Vec::new(),
        }
    }
}
