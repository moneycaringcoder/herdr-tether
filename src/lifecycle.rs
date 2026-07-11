use std::{sync::atomic::AtomicBool, time::Duration as StdDuration};

use anyhow::{Error as AnyError, anyhow};
use thiserror::Error;

use chrono::{DateTime, Duration, Utc};

use crate::{
    backend::{ProcessBinaries, WorkloadState},
    model::SessionId,
    state::{SessionRecord, SessionStatus, StateStore},
    status::{BoundedOutput, run_bounded},
    tmux::TmuxBackend,
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

    pub fn close_owned(&self, id: SessionId) -> Result<CloseOwnedResult, CloseOwnedError> {
        let (target, initial_status) = self.lookup_owned(&id)?;
        let backend = self.backend_for(&id, &target)?;
        let inspected = self.inspect_exact(&backend, &id)?;
        if inspected == WorkloadState::Unknown {
            return Err(CloseOwnedError::WorkloadUnknown(id));
        }
        self.ensure_closing(&id, &target, initial_status)?;
        let workload = match inspected {
            WorkloadState::Missing => ClosedWorkload::Missing,
            WorkloadState::Running { .. } => {
                self.close_exact(&backend, &id)?;
                ClosedWorkload::Terminated
            }
            WorkloadState::Unknown => unreachable!("handled before state transition"),
        };
        self.finish_close(&id, &target)?;
        Ok(CloseOwnedResult { id, workload })
    }

    fn inspect_exact(
        &self,
        backend: &TmuxBackend,
        id: &SessionId,
    ) -> Result<WorkloadState, CloseOwnedError> {
        let spec = backend
            .inspect_exact_spec(id)
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

    fn close_exact(&self, backend: &TmuxBackend, id: &SessionId) -> Result<(), CloseOwnedError> {
        let spec = backend
            .close_exact_spec(id)
            .map_err(|source| CloseOwnedError::Close { id: *id, source })?;
        let cancelled = AtomicBool::new(false);
        match run_bounded(&spec, LIFECYCLE_TRANSPORT_TIMEOUT, &cancelled) {
            BoundedOutput::Completed { status, .. } if status.success() => Ok(()),
            output => Err(CloseOwnedError::Close {
                id: *id,
                source: bounded_transport_error(output),
            }),
        }
    }

    fn lookup_owned(&self, id: &SessionId) -> Result<(String, SessionStatus), CloseOwnedError> {
        let record = self
            .store
            .load()
            .map_err(CloseOwnedError::State)?
            .sessions
            .into_iter()
            .find(|record| record.id == *id)
            .ok_or_else(|| CloseOwnedError::UnknownSession(*id))?;
        if record.status == SessionStatus::Closed {
            return Err(CloseOwnedError::AlreadyClosed(*id));
        }
        Ok((record.target, record.status))
    }

    fn ensure_closing(
        &self,
        id: &SessionId,
        target: &str,
        initial_status: SessionStatus,
    ) -> Result<(), CloseOwnedError> {
        self.store
            .exclusive(|store| {
                let mut state = store.load()?;
                let Some(record) = state.sessions.iter_mut().find(|record| record.id == *id) else {
                    return Ok(false);
                };
                if record.target != target || record.status != initial_status {
                    return Ok(false);
                }
                if record.status == SessionStatus::Active {
                    record.status = SessionStatus::Closing;
                    store.save(&state)?;
                }
                Ok(true)
            })
            .map_err(CloseOwnedError::State)
            .and_then(|transitioned| {
                if transitioned {
                    Ok(())
                } else {
                    Err(CloseOwnedError::ConcurrentModification(*id))
                }
            })
    }

    fn finish_close(&self, id: &SessionId, target: &str) -> Result<(), CloseOwnedError> {
        self.store
            .exclusive(|store| {
                let mut state = store.load()?;
                let Some(record) = state.sessions.iter_mut().find(|record| record.id == *id) else {
                    return Ok(false);
                };
                if record.target != target {
                    return Ok(false);
                }
                if record.status == SessionStatus::Closed {
                    return Ok(true);
                }
                if record.status != SessionStatus::Closing {
                    return Ok(false);
                }
                let now = Utc::now();
                record.status = SessionStatus::Closed;
                record.last_used_at = now;
                record.closed_at = Some(now);
                store.save(&state)?;
                Ok(true)
            })
            .map_err(CloseOwnedError::State)
            .and_then(|finalized| {
                if finalized {
                    Ok(())
                } else {
                    Err(CloseOwnedError::ConcurrentModification(*id))
                }
            })
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
        WorkloadState::Missing => {}
    }

    if record.status == SessionStatus::Active {
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
