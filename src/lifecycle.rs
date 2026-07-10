use chrono::{DateTime, Duration, Utc};

use crate::{
    backend::WorkloadState,
    state::{SessionRecord, SessionStatus},
};

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
