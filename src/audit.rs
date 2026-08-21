//! A bounded record of the lifecycle transitions a workload went through.
//!
//! Lifecycle transitions are what people reconstruct after something goes wrong,
//! and the current state says only where a workload ended up. This keeps the
//! steps, in order, so "it was running this morning and now it is ended" can be
//! answered with what happened in between.
//!
//! It is a sibling of `state.json` rather than part of it: a growing list has
//! nothing to do with the current state, and putting it there would make every
//! state read carry the whole history. It owns its own schema version, its own
//! size ceiling, and its own retention, the way the Agent view preference does.
//!
//! What it deliberately does not hold: a workload's host, target, directory,
//! command, preset, or ownership proof. A trail is meant to survive being read
//! by someone diagnosing a problem, possibly pasted into an issue, so it names
//! the workload and what happened to it and nothing about where the work runs or
//! what it does. The same exclusions `state.json` and notifications already
//! enforce apply here, and more of them.

use std::{
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    model::{SessionId, TmuxSessionId},
    state::SessionStatus,
    storage::{atomic_write_resolved, with_advisory_lock},
};

/// The trail's own schema, independent of `state.json`'s.
pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// The most transitions the trail keeps.
///
/// A ceiling rather than a target: retention removes entries by age first, and
/// this is what stops a busy day from growing the file without limit. Sized to
/// hold several transitions for every workload `state.json` can hold.
pub const MAX_AUDIT_ENTRIES: usize = 4_096;

/// The most the trail may occupy on disk, read or written.
pub const MAX_AUDIT_BYTES: usize = 2 * 1024 * 1024;

/// Why a workload changed state.
///
/// The action is the part a person actually reconstructs: the same end state
/// arrived at by a Stop, by a restart's verification, or by a reconciliation
/// means three different things happened.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// A workload was reserved before its backend was asked to create it.
    Create,
    /// A created workload was confirmed live.
    Activate,
    /// A confirmed Stop began.
    Stop,
    /// A stop finished and the workload's end was recorded.
    Stopped,
    /// A restart reserved a new incarnation.
    Restart,
    /// An observation found the workload alive.
    Observed,
    /// An observation found the workload ended or gone.
    Reconciled,
    /// Retained metadata was removed.
    Removed,
}

impl AuditAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Activate => "activate",
            Self::Stop => "stop",
            Self::Stopped => "stopped",
            Self::Restart => "restart",
            Self::Observed => "observed",
            Self::Reconciled => "reconciled",
            Self::Removed => "removed",
        }
    }
}

/// One transition, as it was committed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    /// When the transition was recorded.
    pub at: DateTime<Utc>,
    /// Which workload. The same identifier `state.json` holds, so a trail can be
    /// read alongside it; nothing here says where that workload runs.
    pub session: SessionId,
    pub action: AuditAction,
    /// The status the record held before the write, when it held one.
    ///
    /// Absent for a create: the workload did not exist, and naming the state it
    /// was about to enter as the state it came from would be a small lie in the
    /// first line of every trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SessionStatus>,
    /// The status the record holds after it.
    pub to: SessionStatus,
    /// Which `tmux` incarnation the transition concerned, when one is known.
    ///
    /// This is what tells a restart apart from the same workload failing twice,
    /// and it is already in `state.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<TmuxSessionId>,
    /// The exit status the workload ended with, when one was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

impl AuditEntry {
    /// One line, for a person reading the trail.
    pub fn summary(&self) -> String {
        let incarnation = self
            .incarnation
            .map(|id| format!(" · {id}"))
            .unwrap_or_default();
        let exit = self
            .exit_status
            .map(|status| format!(" · exit {status}"))
            .unwrap_or_default();
        let from = self
            .from
            .map(|status| format!("{} → ", status_label(status)))
            .unwrap_or_default();
        format!(
            "{}\t{}\t{}\t{from}{}{incarnation}{exit}",
            self.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            self.session,
            self.action.label(),
            status_label(self.to),
        )
    }
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Creating => "creating",
        SessionStatus::Running => "running",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Ended => "ended",
        SessionStatus::Removed => "removed",
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditTrail {
    schema_version: u32,
    entries: Vec<AuditEntry>,
}

impl Default for AuditTrail {
    fn default() -> Self {
        Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Appends transitions to the trail and reads it back.
#[derive(Clone, Debug)]
pub struct AuditStore {
    path: PathBuf,
    /// How long a transition is kept, in days.
    retention_days: u64,
}

impl AuditStore {
    pub fn new(path: PathBuf, retention_days: u64) -> Self {
        Self {
            path,
            retention_days,
        }
    }

    /// Records one transition.
    ///
    /// Best effort by design: the trail is a record of what happened, never the
    /// thing that makes it happen, so a caller that cannot write one must still
    /// have completed the transition. Callers therefore ignore the error rather
    /// than failing an operation that already succeeded.
    pub fn record(&self, entry: AuditEntry) -> Result<()> {
        self.record_at(entry, Utc::now())
    }

    /// Records one transition as of `now`, so retention is testable.
    pub fn record_at(&self, entry: AuditEntry, now: DateTime<Utc>) -> Result<()> {
        with_advisory_lock(&self.path, |path| {
            let mut trail = load_trail(path)?;
            trail.entries.push(entry.clone());
            retain_bounded(&mut trail.entries, self.retention_days, now);
            save_trail(path, &trail)
        })
    }

    /// The trail, oldest first.
    pub fn entries(&self) -> Result<Vec<AuditEntry>> {
        Ok(with_advisory_lock(&self.path, load_trail)?.entries)
    }
}

/// Drops what retention and the ceiling say the trail should no longer hold.
///
/// Age first, because that is the promise; the count is the backstop that keeps
/// a single busy day from growing the file without limit.
fn retain_bounded(entries: &mut Vec<AuditEntry>, retention_days: u64, now: DateTime<Utc>) {
    if let Ok(days) = i64::try_from(retention_days)
        && let Some(cutoff) = chrono::TimeDelta::try_days(days).map(|window| now - window)
    {
        entries.retain(|entry| entry.at >= cutoff);
    }
    if entries.len() > MAX_AUDIT_ENTRIES {
        // The oldest go: a trail is read backwards from whatever just happened.
        let excess = entries.len() - MAX_AUDIT_ENTRIES;
        entries.drain(..excess);
    }
}

fn load_trail(path: &Path) -> Result<AuditTrail> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AuditTrail::default());
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        bail!("audit trail path is not a regular file");
    }
    let size = file.metadata()?.len();
    if size > MAX_AUDIT_BYTES as u64 {
        bail!("audit trail exceeds {MAX_AUDIT_BYTES} bytes");
    }
    let mut source = String::with_capacity(size as usize);
    file.take(MAX_AUDIT_BYTES as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_AUDIT_BYTES {
        bail!("audit trail exceeds {MAX_AUDIT_BYTES} bytes");
    }
    let trail: AuditTrail = serde_json::from_str(&source)?;
    if trail.schema_version != AUDIT_SCHEMA_VERSION {
        bail!("unsupported audit trail schema");
    }
    Ok(trail)
}

fn save_trail(path: &Path, trail: &AuditTrail) -> Result<()> {
    let mut serialized = serde_json::to_vec_pretty(trail)?;
    serialized.push(b'\n');
    if serialized.len() > MAX_AUDIT_BYTES {
        bail!("serialized audit trail exceeds {MAX_AUDIT_BYTES} bytes");
    }
    atomic_write_resolved(path, &serialized).context("save audit trail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(tail: u8) -> SessionId {
        format!("tether-0197f1980000700080000000000000{tail:02}")
            .parse()
            .unwrap()
    }

    fn entry(tail: u8, at: DateTime<Utc>) -> AuditEntry {
        AuditEntry {
            at,
            session: session(tail),
            action: AuditAction::Stopped,
            from: Some(SessionStatus::Stopping),
            to: SessionStatus::Ended,
            incarnation: Some("$7".parse().unwrap()),
            exit_status: Some(0),
        }
    }

    #[test]
    fn a_transition_survives_a_round_trip_and_says_only_what_it_should() {
        let temp = tempfile::tempdir().unwrap();
        let store = AuditStore::new(temp.path().join("audit.json"), 30);
        let now = Utc::now();
        store.record_at(entry(1, now), now).unwrap();
        store
            .record_at(
                AuditEntry {
                    action: AuditAction::Removed,
                    from: Some(SessionStatus::Ended),
                    to: SessionStatus::Removed,
                    exit_status: None,
                    ..entry(1, now)
                },
                now,
            )
            .unwrap();

        let entries = store.entries().unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].action, AuditAction::Stopped);
        assert_eq!(entries[1].to, SessionStatus::Removed);

        // The file itself must not carry where the work runs or what it does.
        let written = std::fs::read_to_string(temp.path().join("audit.json")).unwrap();
        for forbidden in [
            "host",
            "target",
            "directory",
            "command",
            "preset",
            "proof",
            "ownership",
        ] {
            assert!(
                !written.contains(forbidden),
                "the trail must not carry {forbidden}: {written}"
            );
        }
    }

    #[test]
    fn a_create_does_not_claim_a_state_it_came_from() {
        let now = Utc::now();
        let created = AuditEntry {
            action: AuditAction::Create,
            from: None,
            to: SessionStatus::Creating,
            incarnation: None,
            exit_status: None,
            ..entry(1, now)
        };
        let summary = created.summary();
        assert!(
            summary.ends_with("create\tcreating"),
            "a workload that did not exist came from nowhere: {summary}"
        );
        let serialized = serde_json::to_string(&created).unwrap();
        assert!(
            !serialized.contains("\"from\""),
            "an absent origin is absent, not a guess: {serialized}"
        );
    }

    #[test]
    fn retention_drops_transitions_older_than_the_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = AuditStore::new(temp.path().join("audit.json"), 7);
        let now = Utc::now();
        // Written when it was current, read back a fortnight later.
        store
            .record_at(entry(1, now - chrono::Duration::days(14)), now)
            .unwrap();
        assert!(
            store.entries().unwrap().is_empty(),
            "a transition past the window is not kept"
        );

        store.record_at(entry(2, now), now).unwrap();
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn the_trail_cannot_grow_past_its_ceiling() {
        let mut entries: Vec<AuditEntry> = Vec::new();
        let now = Utc::now();
        for index in 0..MAX_AUDIT_ENTRIES + 10 {
            entries.push(entry(1, now - chrono::Duration::seconds(index as i64)));
        }
        let newest = entries.last().cloned().unwrap();
        retain_bounded(&mut entries, 30, now);
        assert_eq!(entries.len(), MAX_AUDIT_ENTRIES);
        assert_eq!(
            entries.last(),
            Some(&newest),
            "the newest transition is the one kept"
        );
    }

    #[test]
    fn an_unsupported_schema_is_refused_rather_than_guessed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("audit.json");
        std::fs::write(&path, "{\"schema_version\":99,\"entries\":[]}\n").unwrap();
        let error = AuditStore::new(path, 30).entries().unwrap_err().to_string();
        assert!(error.contains("unsupported audit trail schema"), "{error}");
    }
}
