use std::{collections::HashSet, sync::atomic::AtomicBool, time::Duration as StdDuration};

use anyhow::{Error as AnyError, anyhow};
use thiserror::Error;

use chrono::{DateTime, Duration, Utc};

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    model::{OwnershipProof, SessionId, TmuxSessionId},
    state::{SessionRecord, SessionStatus, StateStore},
    status::{BoundedOutput, run_bounded},
    tmux::{OWNERSHIP_GUARD_REJECTED, TmuxBackend},
};

/// Exact lifecycle inspection and close transports get three seconds each.
/// The shared bounded runner also caps each captured output stream at 64 KiB.
const LIFECYCLE_TRANSPORT_TIMEOUT: StdDuration = StdDuration::from_secs(3);

/// The disposition of the owned workload when a close completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosedWorkload {
    /// No exact tmux session existed, so only metadata needed finalizing.
    Missing,
    /// The exact tmux session existed and was terminated.
    Terminated,
}

/// Successful result of closing one exact, persisted owned session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloseOwnedResult {
    pub id: SessionId,
    pub workload: ClosedWorkload,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartOwnedResult {
    pub id: SessionId,
    pub identity: TmuxSessionId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveOwnedResult {
    pub id: SessionId,
    pub workload: ClosedWorkload,
}


#[derive(Debug, Error)]
pub enum CloseOwnedError {
    #[error("unknown session `{0}`")]
    UnknownSession(SessionId),
    #[error("session `{0}` is already closed")]
    AlreadyClosed(SessionId),
    #[error("could not determine whether session `{0}` exists")]
    WorkloadUnknown(SessionId),
    #[error("session `{0}` changed while it was closing; retry close")]
    ConcurrentModification(SessionId),
    #[error("access session state")]
    State(#[source] AnyError),
    #[error("configure backend for session `{id}`")]
    BackendConfiguration {
        id: SessionId,
        #[source]
        source: AnyError,
    },
    #[error("inspect session `{id}`")]
    Inspect {
        id: SessionId,
        #[source]
        source: AnyError,
    },
    #[error("close session `{id}`")]
    Close {
        id: SessionId,
        #[source]
        source: AnyError,
    },
    #[error("session `{0}` has no private ownership proof; refusing to inspect or mutate it")]
    MissingOwnershipProof(SessionId),
    #[error("session `{0}` has no retained command and cannot be restarted")]
    MissingCommand(SessionId),
    #[error("session `{id}` cannot be {operation} while it is {status:?}")]
    InvalidStatus {
        id: SessionId,
        operation: &'static str,
        status: SessionStatus,
    },
    #[error("create session `{id}`")]
    Create {
        id: SessionId,
        #[source]
        source: AnyError,
    },
}

/// Coordinates a recoverable close of one exact persisted, owned session.
///
/// An indeterminate inspection does not mutate an active record. Exact inspect
/// and close transports each have a fixed three-second deadline and 64 KiB cap
/// per captured output stream. Once a workload is known to be missing or
/// running, the service revalidates the exact ID, target, and status while
/// persisting `Closing`; it then unlocks before any destructive transport.
/// Finalization revalidates the same fields. No tmux or SSH transport is
/// performed while the advisory state lock is held.
#[derive(Clone, Debug)]
pub struct LifecycleService {
    store: StateStore,
    binaries: ProcessBinaries,
}

impl LifecycleService {
    pub fn new(store: StateStore, binaries: ProcessBinaries) -> Self {
        Self { store, binaries }
    }

    /// Reads the exact persisted record without configuring or invoking transport.
    pub fn owned_record(&self, id: SessionId) -> Result<Option<SessionRecord>, CloseOwnedError> {
        self.store
            .load()
            .map(|state| state.sessions.into_iter().find(|record| record.id == id))
            .map_err(CloseOwnedError::State)
    }
    pub fn stop_owned(&self, id: SessionId) -> Result<CloseOwnedResult, CloseOwnedError> {
        let record = self.lookup_owned(&id)?;
        let ownership_proof = self.require_ownership_proof(&record)?;
        let backend = self.backend_for(&id, &record.target)?;
        let inspected = self.inspect_exact(&backend, &id, &ownership_proof)?;
        if inspected == WorkloadState::Unknown {
            return Err(CloseOwnedError::WorkloadUnknown(id));
        }
        let identity = match inspected {
            WorkloadState::Running { identity, .. } | WorkloadState::Ended { identity, .. } => {
                if record
                    .tmux_session_id
                    .is_some_and(|expected| expected != identity)
                {
                    return Err(CloseOwnedError::ConcurrentModification(id));
                }
                Some(identity)
            }
            WorkloadState::Missing => None,
            WorkloadState::Unknown => unreachable!("handled above"),
        };
        self.ensure_stopping(&record, identity)?;

        let workload = if let Some(identity) = identity {
            match self.inspect_exact(&backend, &id, &ownership_proof)? {
                WorkloadState::Running {
                    identity: current, ..
                }
                | WorkloadState::Ended {
                    identity: current, ..
                } if current == identity => {
                    self.close_exact(&backend, id, &ownership_proof, identity)?;
                    ClosedWorkload::Terminated
                }
                WorkloadState::Missing => ClosedWorkload::Missing,
                WorkloadState::Unknown => return Err(CloseOwnedError::WorkloadUnknown(id)),
                _ => return Err(CloseOwnedError::ConcurrentModification(id)),
            }
        } else {
            ClosedWorkload::Missing
        };
        self.finish_close(&id, &record.target, None)?;
        Ok(CloseOwnedResult { id, workload })
    }

    pub fn close_owned(&self, id: SessionId) -> Result<CloseOwnedResult, CloseOwnedError> {
        self.stop_owned(id)
    }

    fn inspect_exact(
        &self,
        backend: &TmuxBackend,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
    ) -> Result<WorkloadState, CloseOwnedError> {
        let spec = backend
            .inspect_exact_spec(id, ownership_proof)
            .map_err(|source| CloseOwnedError::Inspect { id: *id, source })?;
        let cancelled = AtomicBool::new(false);
        match run_bounded(&spec, LIFECYCLE_TRANSPORT_TIMEOUT, &cancelled) {
            BoundedOutput::Completed {
                status,
                stdout,
                stdout_truncated,
                ..
            } => Ok(backend.classify_exact_inspect_parts(
                id,
                ownership_proof,
                status.code(),
                &stdout,
                stdout_truncated,
            )),
            output => Err(CloseOwnedError::Inspect {
                id: *id,
                source: bounded_transport_error(output),
            }),
        }
    }

    fn close_exact(
        &self,
        backend: &TmuxBackend,
        id: SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<(), CloseOwnedError> {
        let spec = backend
            .close_exact_spec(&id, ownership_proof, identity)
            .map_err(|source| CloseOwnedError::Close { id, source })?;
        let cancelled = AtomicBool::new(false);
        match run_bounded(&spec, LIFECYCLE_TRANSPORT_TIMEOUT, &cancelled) {
            BoundedOutput::Completed { status, stdout, .. }
                if status.success() && stdout.is_empty() =>
            {
                Ok(())
            }
            BoundedOutput::Completed { stdout, .. }
                if String::from_utf8_lossy(&stdout).contains(OWNERSHIP_GUARD_REJECTED) =>
            {
                Err(CloseOwnedError::ConcurrentModification(id))
            }
            output => Err(CloseOwnedError::Close {
                id,
                source: bounded_transport_error(output),
            }),
        }
    }

    fn lookup_owned(&self, id: &SessionId) -> Result<SessionRecord, CloseOwnedError> {

        let record = self
            .store
            .load()
            .map_err(CloseOwnedError::State)?
            .sessions
            .into_iter()
            .find(|record| record.id == *id)
            .ok_or_else(|| CloseOwnedError::UnknownSession(*id))?;
        if matches!(record.status, SessionStatus::Ended | SessionStatus::Removed) {
            return Err(CloseOwnedError::AlreadyClosed(*id));
        }
        Ok(record)
    }
    fn require_ownership_proof(
        &self,
        record: &SessionRecord,
    ) -> Result<OwnershipProof, CloseOwnedError> {
        record
            .ownership_proof
            .ok_or(CloseOwnedError::MissingOwnershipProof(record.id))
    }


    fn ensure_stopping(
        &self,
        snapshot: &SessionRecord,
        identity: Option<TmuxSessionId>,
    ) -> Result<(), CloseOwnedError> {
        self.store
            .exclusive(|store| {
                let mut state = store.load()?;
                let Some(record) = state
                    .sessions
                    .iter_mut()
                    .find(|record| record.id == snapshot.id)
                else {
                    return Ok(false);
                };
                if record != snapshot {
                    return Ok(false);
                }
                if !matches!(
                    record.status,
                    SessionStatus::Running | SessionStatus::Creating | SessionStatus::Stopping
                ) {
                    return Ok(false);
                }
                record.tmux_session_id = identity.or(record.tmux_session_id);
                record.status = SessionStatus::Stopping;
                store.save(&state)?;
                Ok(true)
            })
            .map_err(CloseOwnedError::State)
            .and_then(|transitioned| {
                transitioned
                    .then_some(())
                    .ok_or(CloseOwnedError::ConcurrentModification(snapshot.id))
            })
    }

    fn finish_close(
        &self,
        id: &SessionId,
        target: &str,
        exit_status: Option<i32>,
    ) -> Result<(), CloseOwnedError> {
        self.store
            .exclusive(|store| {
                let mut state = store.load()?;
                let Some(record) = state.sessions.iter_mut().find(|record| record.id == *id) else {
                    return Ok(false);
                };
                if record.target != target {
                    return Ok(false);
                }
                if record.status == SessionStatus::Ended {
                    return Ok(true);
                }
                if record.status != SessionStatus::Stopping {
                    return Ok(false);
                }
                let now = Utc::now();
                record.status = SessionStatus::Ended;
                record.last_used_at = now;
                record.closed_at = Some(now);
                record.exit_status = exit_status;
                store.save(&state)?;
                Ok(true)
            })
            .map_err(CloseOwnedError::State)
            .and_then(|finalized| {
                finalized
                    .then_some(())
                    .ok_or(CloseOwnedError::ConcurrentModification(*id))
            })
    }

    /// Returns an attach command only after proving the exact live incarnation.
    pub fn open_owned(&self, id: SessionId) -> Result<CommandSpec, CloseOwnedError> {
        let record = self
            .owned_record(id)?
            .ok_or(CloseOwnedError::UnknownSession(id))?;
        if record.status != SessionStatus::Running {
            return Err(CloseOwnedError::InvalidStatus {
                id,
                operation: "opened",
                status: record.status,
            });
        }
        let ownership_proof = self.require_ownership_proof(&record)?;
        let backend = self.backend_for(&id, &record.target)?;
        let identity = match self.inspect_exact(&backend, &id, &ownership_proof)? {
            WorkloadState::Running { identity, .. } => identity,
            WorkloadState::Unknown => return Err(CloseOwnedError::WorkloadUnknown(id)),
            WorkloadState::Missing | WorkloadState::Ended { .. } => {
                return Err(CloseOwnedError::InvalidStatus {
                    id,
                    operation: "opened",
                    status: SessionStatus::Ended,
                });
            }
        };
        if record
            .tmux_session_id
            .is_some_and(|expected| expected != identity)
        {
            return Err(CloseOwnedError::ConcurrentModification(id));
        }
        self.store
            .update(|state| {
                let current = state
                    .sessions
                    .iter_mut()
                    .find(|current| current.id == id)
                    .ok_or_else(|| anyhow!("session disappeared"))?;
                if current != &record {
                    return Err(anyhow!("session changed"));
                }
                current.tmux_session_id = Some(identity);
                current.last_used_at = Utc::now();
                Ok(())
            })
            .map_err(|_| CloseOwnedError::ConcurrentModification(id))?;
        match self.inspect_exact(&backend, &id, &ownership_proof)? {
            WorkloadState::Running {
                identity: current, ..
            } if current == identity => backend
                .attach_command(&id, &ownership_proof, identity)
                .map_err(|source| CloseOwnedError::Inspect { id, source }),
            WorkloadState::Unknown => Err(CloseOwnedError::WorkloadUnknown(id)),
            _ => Err(CloseOwnedError::ConcurrentModification(id)),
        }
    }

    /// Reconciles persisted metadata with a truthful exact tmux observation.
    pub fn observe_owned(&self, id: SessionId) -> Result<WorkloadState, CloseOwnedError> {
        let record = self
            .owned_record(id)?
            .ok_or(CloseOwnedError::UnknownSession(id))?;
        let ownership_proof = self.require_ownership_proof(&record)?;
        let backend = self.backend_for(&id, &record.target)?;
        let observed = self.inspect_exact(&backend, &id, &ownership_proof)?;
        match observed {
            WorkloadState::Unknown => return Err(CloseOwnedError::WorkloadUnknown(id)),
            WorkloadState::Running { identity, .. } => {
                if record
                    .tmux_session_id
                    .is_some_and(|expected| expected != identity)
                {
                    return Err(CloseOwnedError::ConcurrentModification(id));
                }
                if matches!(record.status, SessionStatus::Creating | SessionStatus::Running) {
                    self.store
                        .update(|state| {
                            let current = state
                                .sessions
                                .iter_mut()
                                .find(|current| current.id == id)
                                .ok_or_else(|| anyhow!("session disappeared"))?;
                            if current != &record {
                                return Err(anyhow!("session changed"));
                            }
                            current.status = SessionStatus::Running;
                            current.tmux_session_id = Some(identity);
                            current.closed_at = None;
                            current.exit_status = None;
                            Ok(())
                        })
                        .map_err(|_| CloseOwnedError::ConcurrentModification(id))?;
                }
            }
            WorkloadState::Ended {
                identity,
                exit_status,
            } => {
                if record
                    .tmux_session_id
                    .is_some_and(|expected| expected != identity)
                {
                    return Err(CloseOwnedError::ConcurrentModification(id));
                }
                self.mark_ended(&record, Some(identity), exit_status)?;
            }
            WorkloadState::Missing => self.mark_ended(&record, None, None)?,
        }
        Ok(observed)
    }

    /// Restarts an ended workload from its privately retained exact command.
    ///
    /// A `Creating` reservation survives every failure. Retrying promotes an
    /// already-created exact session instead of creating a duplicate.
    pub fn restart_owned(&self, id: SessionId) -> Result<RestartOwnedResult, CloseOwnedError> {
        let record = self
            .owned_record(id)?
            .ok_or(CloseOwnedError::UnknownSession(id))?;
        let ownership_proof = self.require_ownership_proof(&record)?;
        if record.status == SessionStatus::Removed {
            return Err(CloseOwnedError::InvalidStatus {
                id,
                operation: "restarted",
                status: record.status,
            });
        }
        let command = record
            .command
            .clone()
            .ok_or(CloseOwnedError::MissingCommand(id))?;
        let backend = self.backend_for(&id, &record.target)?;
        match self.inspect_exact(&backend, &id, &ownership_proof)? {
            WorkloadState::Running { identity, .. } => {
                if record.status != SessionStatus::Creating
                    && record.tmux_session_id != Some(identity)
                {
                    return Err(CloseOwnedError::ConcurrentModification(id));
                }
                self.promote_running(&record, identity)?;
                return Ok(RestartOwnedResult { id, identity });
            }
            WorkloadState::Ended { identity, .. } => {
                if record
                    .tmux_session_id
                    .is_some_and(|expected| expected != identity)
                {
                    return Err(CloseOwnedError::ConcurrentModification(id));
                }
                match self.inspect_exact(&backend, &id, &ownership_proof)? {
                    WorkloadState::Ended {
                        identity: current, ..
                    } if current == identity => {
                        self.close_exact(&backend, id, &ownership_proof, identity)?
                    }
                    WorkloadState::Missing => {}
                    WorkloadState::Unknown => {
                        return Err(CloseOwnedError::WorkloadUnknown(id));
                    }
                    _ => return Err(CloseOwnedError::ConcurrentModification(id)),
                }
            }
            WorkloadState::Missing => {}
            WorkloadState::Unknown => return Err(CloseOwnedError::WorkloadUnknown(id)),
        }
        if !matches!(record.status, SessionStatus::Ended | SessionStatus::Creating) {
            return Err(CloseOwnedError::InvalidStatus {
                id,
                operation: "restarted",
                status: record.status,
            });
        }
        self.store
            .update(|state| {
                let current = state
                    .sessions
                    .iter_mut()
                    .find(|current| current.id == id)
                    .ok_or_else(|| anyhow!("session disappeared"))?;
                if current != &record {
                    return Err(anyhow!("session changed"));
                }
                current.status = SessionStatus::Creating;
                current.tmux_session_id = None;
                current.closed_at = None;
                current.exit_status = None;
                Ok(())
            })
            .map_err(|_| CloseOwnedError::ConcurrentModification(id))?;
        let reservation = self
            .owned_record(id)?
            .ok_or(CloseOwnedError::UnknownSession(id))?;
        let identity = backend
            .create(&LaunchSpec {
                id,
                ownership_proof,
                directory: reservation.directory.clone(),
                command,
            })
            .map_err(|source| CloseOwnedError::Create { id, source })?;
        self.promote_running(&reservation, identity)?;
        Ok(RestartOwnedResult { id, identity })
    }

    /// Finalizes ended metadata and removes only an exact dead tmux incarnation.
    pub fn remove_owned(&self, id: SessionId) -> Result<RemoveOwnedResult, CloseOwnedError> {
        let record = self
            .owned_record(id)?
            .ok_or(CloseOwnedError::UnknownSession(id))?;
        let ownership_proof = self.require_ownership_proof(&record)?;
        if record.status == SessionStatus::Removed {
            return Ok(RemoveOwnedResult {
                id,
                workload: ClosedWorkload::Missing,
            });
        }
        if record.status != SessionStatus::Ended {
            return Err(CloseOwnedError::InvalidStatus {
                id,
                operation: "removed",
                status: record.status,
            });
        }
        let backend = self.backend_for(&id, &record.target)?;
        let workload = match self.inspect_exact(&backend, &id, &ownership_proof)? {
            WorkloadState::Ended { identity, .. }
                if record.tmux_session_id.is_none()
                    || record.tmux_session_id == Some(identity) =>
            {
                match self.inspect_exact(&backend, &id, &ownership_proof)? {
                    WorkloadState::Ended {
                        identity: current, ..
                    } if current == identity => {
                        self.close_exact(&backend, id, &ownership_proof, identity)?
                    }
                    WorkloadState::Missing => {}
                    WorkloadState::Unknown => {
                        return Err(CloseOwnedError::WorkloadUnknown(id));
                    }
                    _ => return Err(CloseOwnedError::ConcurrentModification(id)),
                }
                ClosedWorkload::Terminated
            }
            WorkloadState::Missing => ClosedWorkload::Missing,
            WorkloadState::Unknown => return Err(CloseOwnedError::WorkloadUnknown(id)),
            _ => return Err(CloseOwnedError::ConcurrentModification(id)),
        };
        self.store
            .update(|state| {
                let current = state
                    .sessions
                    .iter_mut()
                    .find(|current| current.id == id)
                    .ok_or_else(|| anyhow!("session disappeared"))?;
                if current != &record {
                    return Err(anyhow!("session changed"));
                }
                current.status = SessionStatus::Removed;
                current.last_used_at = Utc::now();
                current.closed_at.get_or_insert(current.last_used_at);
                Ok(())
            })
            .map_err(|_| CloseOwnedError::ConcurrentModification(id))?;
        Ok(RemoveOwnedResult { id, workload })
    }

    fn mark_ended(
        &self,
        snapshot: &SessionRecord,
        identity: Option<TmuxSessionId>,
        exit_status: Option<i32>,
    ) -> Result<(), CloseOwnedError> {
        if snapshot.status == SessionStatus::Removed {
            return Ok(());
        }
        self.store
            .update(|state| {
                let current = state
                    .sessions
                    .iter_mut()
                    .find(|current| current.id == snapshot.id)
                    .ok_or_else(|| anyhow!("session disappeared"))?;
                if current != snapshot {
                    return Err(anyhow!("session changed"));
                }
                let now = Utc::now();
                current.status = SessionStatus::Ended;
                current.tmux_session_id = identity.or(current.tmux_session_id);
                current.last_used_at = now;
                current.closed_at.get_or_insert(now);
                current.exit_status = exit_status;
                Ok(())
            })
            .map_err(|_| CloseOwnedError::ConcurrentModification(snapshot.id))
    }

    fn promote_running(
        &self,
        snapshot: &SessionRecord,
        identity: TmuxSessionId,
    ) -> Result<(), CloseOwnedError> {
        self.store
            .update(|state| {
                let current = state
                    .sessions
                    .iter_mut()
                    .find(|current| current.id == snapshot.id)
                    .ok_or_else(|| anyhow!("session disappeared"))?;
                if current != snapshot {
                    return Err(anyhow!("session changed"));
                }
                current.status = SessionStatus::Running;
                current.tmux_session_id = Some(identity);
                current.closed_at = None;
                current.exit_status = None;
                current.last_used_at = Utc::now();
                Ok(())
            })
            .map_err(|_| CloseOwnedError::ConcurrentModification(snapshot.id))
    }

    fn backend_for(&self, id: &SessionId, target: &str) -> Result<TmuxBackend, CloseOwnedError> {
        if target == "local" {
            Ok(TmuxBackend::local(self.binaries.clone()))
        } else {
            TmuxBackend::remote(target.to_owned(), self.binaries.clone())
                .map_err(|source| CloseOwnedError::BackendConfiguration { id: *id, source })
        }
    }
}

fn bounded_transport_error(output: BoundedOutput) -> AnyError {
    match output {
        BoundedOutput::TimedOut => anyhow!(
            "transport timed out after {} seconds",
            LIFECYCLE_TRANSPORT_TIMEOUT.as_secs()
        ),
        BoundedOutput::Cancelled => anyhow!("transport was cancelled"),
        BoundedOutput::SpawnError(kind) => {
            anyhow!("spawn transport: {}", std::io::Error::from(kind))
        }
        BoundedOutput::Error => anyhow!("transport failed"),
        BoundedOutput::Completed {
            status,
            stderr,
            stderr_truncated,
            ..
        } => {
            let detail = sanitize_transport_detail(&String::from_utf8_lossy(&stderr));
            let truncation = if stderr_truncated {
                " [stderr truncated]"
            } else {
                ""
            };
            if detail.is_empty() {
                anyhow!("transport failed with status {status}{truncation}")
            } else {
                anyhow!("transport failed with status {status}: {detail}{truncation}")
            }
        }
    }
}

fn sanitize_transport_detail(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut sanitized = String::with_capacity(text.len().min(MAX_CHARS));
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for sequence_character in characters.by_ref() {
                        if ('@'..='~').contains(&sequence_character) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(sequence_character) = characters.next() {
                        if sequence_character == '\u{7}' {
                            break;
                        }
                        if sequence_character == '\u{1b}' {
                            let _ = characters.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let mut bounded = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// An immutable set of exact persisted records selected for a possible prune.
///
/// The candidate snapshots are intentionally private: callers can display the
/// IDs but cannot widen or alter the set that [`PruneService::apply`] will
/// revalidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrunePreview {
    captured_at: DateTime<Utc>,
    retention_days: u64,
    candidate_ids: Vec<SessionId>,
    candidates: Vec<SessionRecord>,
}

impl PrunePreview {
    pub fn captured_at(&self) -> DateTime<Utc> {
        self.captured_at
    }

    pub fn older_than_days(&self) -> u64 {
        self.retention_days
    }

    pub fn ids(&self) -> &[SessionId] {
        &self.candidate_ids
    }
}

/// The truthful outcome of applying a previously captured prune preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneResult {
    pub removed_ids: Vec<SessionId>,
    pub skipped_ids: Vec<SessionId>,
}
/// Finalized removed metadata is retained for this many days by default.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;


#[derive(Debug, Error)]
pub enum PruneError {
    #[error("retention of {0} days is too large")]
    RetentionTooLarge(u64),
    #[error("access session state")]
    State(#[source] AnyError),
}

/// Previews and atomically applies closed-metadata pruning using state only.
#[derive(Clone, Debug)]
pub struct PruneService {
    store: StateStore,
}

impl PruneService {
    pub fn new(store: StateStore) -> Self {
        Self { store }
    }

    /// Captures one wall-clock instant and one immutable candidate snapshot.
    pub fn preview(&self, retention_days: u64) -> Result<PrunePreview, PruneError> {
        self.preview_at(retention_days, Utc::now())
    }

    /// Captures a preview at an explicit instant.
    ///
    /// This is useful to make cutoff-boundary decisions deterministic while
    /// retaining the same conversion and persistence behavior as [`Self::preview`].
    pub fn preview_at(
        &self,
        retention_days: u64,
        captured_at: DateTime<Utc>,
    ) -> Result<PrunePreview, PruneError> {
        let retention = retention_duration(retention_days)?;
        let state = self.store.load().map_err(PruneError::State)?;
        let candidates = state
            .sessions
            .into_iter()
            .filter(|record| {
                cleanup_eligibility(record, WorkloadState::Missing, captured_at, retention)
                    == CleanupEligibility::RemoveMetadata
            })
            .collect::<Vec<_>>();
        let candidate_ids = candidates.iter().map(|record| record.id).collect();
        Ok(PrunePreview {
            captured_at,
            retention_days,
            candidate_ids,
            candidates,
        })
    }
    /// Applies only unchanged, still-eligible records from `preview`.
    ///
    /// Loading, revalidation, removal, and persistence occur under one state
    /// update lock. Missing, changed, or no-longer-eligible candidates are
    /// reported as skipped and do not prevent independent valid candidates
    /// from being removed.
    pub fn apply(&self, preview: &PrunePreview) -> Result<PruneResult, PruneError> {
        let retention = retention_duration(preview.retention_days)?;
        let apply_at = Utc::now();
        self.store
            .update(|state| {
                let mut removed_ids = Vec::with_capacity(preview.candidates.len());
                let mut skipped_ids = Vec::new();
                for snapshot in &preview.candidates {
                    let unchanged_and_eligible = state
                        .sessions
                        .iter()
                        .find(|record| record.id == snapshot.id)
                        .is_some_and(|record| {
                            record == snapshot
                                && cleanup_eligibility(
                                    record,
                                    WorkloadState::Missing,
                                    apply_at,
                                    retention,
                                ) == CleanupEligibility::RemoveMetadata
                        });
                    if unchanged_and_eligible {
                        removed_ids.push(snapshot.id);
                    } else {
                        skipped_ids.push(snapshot.id);
                    }
                }
                let removed = removed_ids.iter().copied().collect::<HashSet<_>>();
                state
                    .sessions
                    .retain(|record| !removed.contains(&record.id));
                Ok(PruneResult {
                    removed_ids,
                    skipped_ids,
                })
            })
            .map_err(PruneError::State)
    }
    /// Automatically removes only finalized `Removed` metadata after retention.
    ///
    /// This path performs no backend operation and therefore cannot terminate
    /// or otherwise mutate any workload.
    pub fn automatic_cleanup(&self) -> Result<Vec<SessionId>, PruneError> {
        self.automatic_cleanup_at(Utc::now())
    }

    pub fn automatic_cleanup_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<SessionId>, PruneError> {
        let retention = retention_duration(DEFAULT_RETENTION_DAYS)?;
        self.store
            .update(|state| {
                let removed = state
                    .sessions
                    .iter()
                    .filter(|record| {
                        record.status == SessionStatus::Removed
                            && cleanup_eligibility(
                                record,
                                WorkloadState::Missing,
                                now,
                                retention,
                            ) == CleanupEligibility::RemoveMetadata
                    })
                    .map(|record| record.id)
                    .collect::<Vec<_>>();
                let removed_set = removed.iter().copied().collect::<HashSet<_>>();
                state
                    .sessions
                    .retain(|record| !removed_set.contains(&record.id));
                Ok(removed)
            })
            .map_err(PruneError::State)
    }
}

fn retention_duration(days: u64) -> Result<Duration, PruneError> {
    let days = i64::try_from(days).map_err(|_| PruneError::RetentionTooLarge(days))?;
    Duration::try_days(days).ok_or(PruneError::RetentionTooLarge(days as u64))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupEligibility {
    KeepActive,
    KeepUnknown,
    KeepRecent,
    RemoveMetadata,
}

/// Determines whether closed, missing session metadata is old enough to prune.
///
/// A workload that is still running always wins over metadata state. Unknown
/// probes and malformed closed records are retained conservatively.
pub fn cleanup_eligibility(
    record: &SessionRecord,
    workload: WorkloadState,
    now: DateTime<Utc>,
    retention: Duration,
) -> CleanupEligibility {
    match workload {
        WorkloadState::Running { .. } => return CleanupEligibility::KeepActive,
        WorkloadState::Unknown => return CleanupEligibility::KeepUnknown,
        WorkloadState::Missing | WorkloadState::Ended { .. } => {}
    }

    if !matches!(
        record.status,
        SessionStatus::Ended | SessionStatus::Removed
    ) {
        return CleanupEligibility::KeepActive;
    }

    let Some(closed_at) = record.closed_at else {
        return CleanupEligibility::KeepRecent;
    };
    let Some(cutoff) = now.checked_sub_signed(retention) else {
        return CleanupEligibility::KeepRecent;
    };
    if retention < Duration::zero() || closed_at > cutoff {
        CleanupEligibility::KeepRecent
    } else {
        CleanupEligibility::RemoveMetadata
    }
}
