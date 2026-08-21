use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    agent_view::GROUP_TOKEN,
    backend::ProcessBinaries,
    config::{ConfigStore, NotificationDefaults},
    herdr::{HerdrClient, HerdrContext, PaneTitle},
    herdr_socket::{HerdrSessionSnapshot, HerdrSocketClient, NotificationSound},
    lifecycle::{ClosedWorkload, LifecycleService},
    mission_control::{
        MemberTarget, MissionAgentState, MissionControlService, TargetDelivery,
        label_materialized_member, resolve_binding,
    },
    model::{
        OrchestrationGroupId, OrchestrationMembershipId, OrchestrationTitle, OwnershipProof,
        Placement, SessionId, TmuxSessionId,
    },
    observer::{
        AttentionReason, ObserverAction, ObserverAgentState, ObserverCapabilities, ObserverCapture,
        ObserverInputKind, ObserverKey, ObserverLifecycle, ObserverOutcome, ObserverState,
        ObserverWorker, StaleReason, WorkerAttention, render,
    },
    paths::AppPaths,
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, StateStore,
    },
    tmux::TmuxBackend,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrchestrationWorkerSpec {
    pub session_id: SessionId,
    pub title: Option<OrchestrationTitle>,
    pub capabilities: OrchestrationCapabilities,
}

pub const MANAGER_STALE_GROUP_ERROR: &str =
    "Observer group changed while the manager was open; refresh and retry";
const OBSERVER_REFRESH_FAILURE_NOTICE: &str =
    "Refresh failed; stale output retained · r retry · q back";
const OBSERVER_GROUP_DELETED_ERROR: &str = "Observer group was deleted; access revoked";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObserverAuthorityOutcome {
    Authorized,
    RecoverableFailure,
    GroupDeleted,
}

/// A lifecycle action asked of every worker in a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupAction {
    Stop,
    Restart,
}

impl GroupAction {
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// Why a member is not part of the act.
///
/// Each one is a refusal, not a failure: the workload is left exactly as it was,
/// and the reason says what would have to change first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupSkip {
    /// No record: the workload was removed or never existed.
    MissingRecord,
    /// A legacy record with no private ownership proof. Tether cannot prove the
    /// workload is its own, so a group is not a way to act on it.
    NoOwnershipProof,
    /// Not running, so there is nothing to stop.
    NotRunning,
    /// Not ended, so a restart would be a second incarnation of live work.
    NotEnded,
    /// Ended without a retained command, so there is nothing to restart.
    MissingCommand,
    /// Failed immediately, and its restart is still paced.
    RestartPaced { seconds: i64 },
    /// The membership changed between the plan and the act.
    MembershipChanged,
}

impl GroupSkip {
    pub fn reason(self) -> String {
        match self {
            Self::MissingRecord => "no record; it was removed".to_owned(),
            Self::NoOwnershipProof => {
                "legacy record with no ownership proof; recreate it to manage it".to_owned()
            }
            Self::NotRunning => "not running".to_owned(),
            Self::NotEnded => "still running; stop it first".to_owned(),
            Self::MissingCommand => "no retained command".to_owned(),
            Self::RestartPaced { seconds } => {
                format!("failed immediately; restart paced for {seconds}s")
            }
            Self::MembershipChanged => "membership changed since the plan".to_owned(),
        }
    }
}

/// Whether a member is in the act or out of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupDecision {
    Act,
    Skip(GroupSkip),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupPlanEntry {
    pub session_id: SessionId,
    /// The membership this decision was made for, so a remove and re-add
    /// between the plan and the act is caught rather than obeyed.
    pub membership_id: OrchestrationMembershipId,
    pub decision: GroupDecision,
}

/// What a group action would do, resolved against the records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupPlan {
    pub group_id: OrchestrationGroupId,
    pub action: GroupAction,
    pub entries: Vec<GroupPlanEntry>,
}

impl GroupPlan {
    /// The members the act would touch.
    pub fn acting(&self) -> impl Iterator<Item = &GroupPlanEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.decision == GroupDecision::Act)
    }

    /// The members it would leave alone, with the reason for each.
    pub fn skipped(&self) -> impl Iterator<Item = (SessionId, GroupSkip)> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.decision {
                GroupDecision::Skip(reason) => Some((entry.session_id, reason)),
                GroupDecision::Act => None,
            })
    }
}

/// What happened to one member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupMemberResult {
    Stopped(ClosedWorkload),
    Restarted,
    Skipped(GroupSkip),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupOutcome {
    pub session_id: SessionId,
    pub result: GroupMemberResult,
}

/// What a confirmed group action actually did, member by member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupActionReport {
    pub group_id: OrchestrationGroupId,
    pub action: GroupAction,
    pub outcomes: Vec<GroupOutcome>,
}

impl GroupActionReport {
    pub fn acted(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome.result,
                    GroupMemberResult::Stopped(_) | GroupMemberResult::Restarted
                )
            })
            .count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.result, GroupMemberResult::Failed(_)))
            .count()
    }

    pub fn skipped(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.result, GroupMemberResult::Skipped(_)))
            .count()
    }
}

/// Whether one member is eligible for the action, from its record alone.
fn plan_member(action: GroupAction, record: Option<&SessionRecord>) -> GroupDecision {
    let Some(record) = record else {
        return GroupDecision::Skip(GroupSkip::MissingRecord);
    };
    // The single-workload path refuses a proofless record before any transport;
    // saying so here keeps the confirmation honest instead of promising an act
    // that would be refused.
    if record.ownership_proof.is_none() {
        return GroupDecision::Skip(GroupSkip::NoOwnershipProof);
    }
    match action {
        GroupAction::Stop if record.status == SessionStatus::Running => GroupDecision::Act,
        GroupAction::Stop => GroupDecision::Skip(GroupSkip::NotRunning),
        GroupAction::Restart if record.status != SessionStatus::Ended => {
            GroupDecision::Skip(GroupSkip::NotEnded)
        }
        GroupAction::Restart if record.command.is_none() => {
            GroupDecision::Skip(GroupSkip::MissingCommand)
        }
        GroupAction::Restart => match record.paced_restart_until() {
            // The pace withholds an explicit single-workload restart, so a group
            // must not become the way around it.
            Some(until) => {
                let remaining = until.signed_duration_since(Utc::now());
                if remaining > chrono::TimeDelta::zero() {
                    GroupDecision::Skip(GroupSkip::RestartPaced {
                        seconds: remaining.num_seconds().max(1),
                    })
                } else {
                    GroupDecision::Act
                }
            }
            None => GroupDecision::Act,
        },
    }
}

/// Keeps a member's failure text to one bounded, terminal-safe line.
fn sanitize_group_failure(error: &str) -> String {
    let collapsed = error
        .split_whitespace()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(240).collect()
}

/// State-only management for opt-in orchestration groups.
#[derive(Clone, Debug)]
pub struct OrchestrationService {
    store: StateStore,
}

impl OrchestrationService {
    pub fn new(store: StateStore) -> Self {
        Self { store }
    }

    pub fn create_group(
        &self,
        id: OrchestrationGroupId,
        title: OrchestrationTitle,
        orchestrator_session_id: SessionId,
    ) -> Result<OrchestrationGroup> {
        self.store.update(|state| {
            require_role_eligible_session(state, orchestrator_session_id, "orchestrator")?;
            if state
                .orchestration_groups
                .iter()
                .any(|group| group.id == id)
            {
                bail!("orchestration group `{id}` already exists");
            }
            if state.orchestration_groups.len() >= State::MAX_ORCHESTRATION_GROUPS {
                bail!(
                    "orchestration group limit of {} has been reached",
                    State::MAX_ORCHESTRATION_GROUPS
                );
            }
            let group = OrchestrationGroup {
                id,
                title,
                orchestrator_session_id,
                workers: Vec::new(),
            };
            state.orchestration_groups.push(group.clone());
            Ok(group)
        })
    }

    pub fn create_group_with_workers(
        &self,
        id: OrchestrationGroupId,
        title: OrchestrationTitle,
        orchestrator_session_id: SessionId,
        workers: Vec<OrchestrationWorkerSpec>,
    ) -> Result<OrchestrationGroup> {
        validate_worker_specs(orchestrator_session_id, &workers)?;
        self.store.update(|state| {
            require_role_eligible_session(state, orchestrator_session_id, "orchestrator")?;
            for worker in &workers {
                require_role_eligible_session(state, worker.session_id, "worker")?;
            }
            if state
                .orchestration_groups
                .iter()
                .any(|group| group.id == id)
            {
                bail!("orchestration group `{id}` already exists");
            }
            if state.orchestration_groups.len() >= State::MAX_ORCHESTRATION_GROUPS {
                bail!(
                    "orchestration group limit of {} has been reached",
                    State::MAX_ORCHESTRATION_GROUPS
                );
            }
            let group = OrchestrationGroup {
                id,
                title,
                orchestrator_session_id,
                workers: worker_members(Vec::new(), workers),
            };
            state.orchestration_groups.push(group.clone());
            Ok(group)
        })
    }

    pub fn replace_workers(
        &self,
        expected: &OrchestrationGroup,
        workers: Vec<OrchestrationWorkerSpec>,
    ) -> Result<OrchestrationGroup> {
        validate_worker_specs(expected.orchestrator_session_id, &workers)?;
        self.store.update(|state| {
            let index = state
                .orchestration_groups
                .iter()
                .position(|group| group.id == expected.id)
                .with_context(|| format!("unknown orchestration group `{}`", expected.id))?;
            let current = state.orchestration_groups[index].clone();
            if &current != expected {
                bail!(MANAGER_STALE_GROUP_ERROR);
            }
            let retained = current
                .workers
                .iter()
                .map(|worker| worker.session_id)
                .collect::<HashSet<_>>();
            for worker in &workers {
                if !retained.contains(&worker.session_id) {
                    require_role_eligible_session(state, worker.session_id, "new worker")?;
                }
            }
            let group = &mut state.orchestration_groups[index];
            group.workers = worker_members(std::mem::take(&mut group.workers), workers);
            Ok(group.clone())
        })
    }

    pub fn delete_group(&self, id: &OrchestrationGroupId) -> Result<OrchestrationGroup> {
        self.store.update(|state| {
            let index = state
                .orchestration_groups
                .iter()
                .position(|group| &group.id == id)
                .with_context(|| format!("unknown orchestration group `{id}`"))?;
            Ok(state.orchestration_groups.remove(index))
        })
    }

    pub fn delete_group_if_unchanged(
        &self,
        expected: &OrchestrationGroup,
    ) -> Result<OrchestrationGroup> {
        self.store.update(|state| {
            let index = state
                .orchestration_groups
                .iter()
                .position(|group| group.id == expected.id)
                .with_context(|| format!("unknown orchestration group `{}`", expected.id))?;
            if &state.orchestration_groups[index] != expected {
                bail!(MANAGER_STALE_GROUP_ERROR);
            }
            Ok(state.orchestration_groups.remove(index))
        })
    }

    pub fn list_groups(&self) -> Result<Vec<OrchestrationGroup>> {
        Ok(self.store.load()?.orchestration_groups)
    }

    pub fn group(&self, id: &OrchestrationGroupId) -> Result<OrchestrationGroup> {
        self.store
            .load()?
            .orchestration_groups
            .into_iter()
            .find(|group| &group.id == id)
            .with_context(|| format!("unknown orchestration group `{id}`"))
    }

    pub fn add_worker(
        &self,
        group_id: &OrchestrationGroupId,
        session_id: SessionId,
        title: Option<OrchestrationTitle>,
        capabilities: OrchestrationCapabilities,
    ) -> Result<OrchestrationMember> {
        if capabilities.is_empty() {
            bail!("worker `{session_id}` must declare at least one capability");
        }
        self.store.update(|state| {
            require_role_eligible_session(state, session_id, "worker")?;
            let group = state
                .orchestration_groups
                .iter_mut()
                .find(|group| &group.id == group_id)
                .with_context(|| format!("unknown orchestration group `{group_id}`"))?;
            if group.orchestrator_session_id == session_id {
                bail!("orchestrator must not also be a worker");
            }
            if group
                .workers
                .iter()
                .any(|worker| worker.session_id == session_id)
            {
                bail!(
                    "worker `{session_id}` is already a member of orchestration group `{group_id}`"
                );
            }
            if group.workers.len() >= OrchestrationGroup::MAX_WORKERS {
                bail!(
                    "orchestration worker limit of {} has been reached for group `{group_id}`",
                    OrchestrationGroup::MAX_WORKERS
                );
            }
            let worker = OrchestrationMember {
                session_id,
                membership_id: OrchestrationMembershipId::new(),
                title,
                capabilities,
            };
            group.workers.push(worker.clone());
            Ok(worker)
        })
    }

    pub fn reassign_orchestrator(
        &self,
        expected: &OrchestrationGroup,
        new_session_id: SessionId,
    ) -> Result<OrchestrationGroup> {
        self.store.update(|state| {
            let index = state
                .orchestration_groups
                .iter()
                .position(|group| group.id == expected.id)
                .with_context(|| format!("unknown orchestration group `{}`", expected.id))?;
            if &state.orchestration_groups[index] != expected {
                bail!(MANAGER_STALE_GROUP_ERROR);
            }
            require_role_eligible_session(state, new_session_id, "orchestrator")?;

            let group = &mut state.orchestration_groups[index];
            group.orchestrator_session_id = new_session_id;
            group
                .workers
                .retain(|worker| worker.session_id != new_session_id);
            Ok(group.clone())
        })
    }

    pub fn remove_worker(
        &self,
        group_id: &OrchestrationGroupId,
        session_id: SessionId,
    ) -> Result<OrchestrationMember> {
        self.store.update(|state| {
            let group = state
                .orchestration_groups
                .iter_mut()
                .find(|group| &group.id == group_id)
                .with_context(|| format!("unknown orchestration group `{group_id}`"))?;
            let index = group
                .workers
                .iter()
                .position(|worker| worker.session_id == session_id)
                .with_context(|| {
                    format!(
                        "worker `{session_id}` is not a member of orchestration group `{group_id}`"
                    )
                })?;
            Ok(group.workers.remove(index))
        })
    }

    /// What a group-wide stop or restart would do, without contacting a host.
    ///
    /// Membership is metadata: a group can hold a reference to a workload that
    /// has since ended, lost its ownership proof, or disappeared entirely, and
    /// state validation deliberately does not join the two. So a plan resolves
    /// every member against the records first and says which ones it would act
    /// on, which keeps the confirmation honest about the size of the act.
    pub fn plan_group_action(
        &self,
        group_id: &OrchestrationGroupId,
        action: GroupAction,
    ) -> Result<GroupPlan> {
        let state = self.store.load()?;
        let group = state
            .orchestration_groups
            .iter()
            .find(|group| &group.id == group_id)
            .with_context(|| format!("unknown orchestration group `{group_id}`"))?;
        let entries = group
            .workers
            .iter()
            .map(|member| {
                let record = state
                    .sessions
                    .iter()
                    .find(|record| record.id == member.session_id);
                GroupPlanEntry {
                    session_id: member.session_id,
                    membership_id: member.membership_id,
                    decision: plan_member(action, record),
                }
            })
            .collect();
        Ok(GroupPlan {
            group_id: group_id.clone(),
            action,
            entries,
        })
    }

    /// Runs a confirmed plan, one member at a time, through the single-workload
    /// path.
    ///
    /// Every ownership decision stays where it already lives: this calls the
    /// same [`LifecycleService`] operations the picker and `session` commands
    /// call, so the proof requirement, the exact re-inspections, and the
    /// execution-time `tmux` guard all apply per member. A group is a list of
    /// workloads to ask about, never an authority of its own.
    ///
    /// Only members named in the plan are acted on, and only while their
    /// membership is unchanged, so a group edited after the confirmation cannot
    /// enlarge or redirect what was confirmed. One member's failure does not
    /// abandon the rest; each result is reported on its own.
    pub fn apply_group_action(
        &self,
        plan: &GroupPlan,
        lifecycle: &LifecycleService,
    ) -> Result<GroupActionReport> {
        let mut outcomes = Vec::with_capacity(plan.entries.len());
        for entry in &plan.entries {
            let GroupDecision::Act = entry.decision else {
                let GroupDecision::Skip(reason) = entry.decision else {
                    unreachable!("a decision is either an action or a skip");
                };
                outcomes.push(GroupOutcome {
                    session_id: entry.session_id,
                    result: GroupMemberResult::Skipped(reason),
                });
                continue;
            };
            // A remove and re-add between the confirmation and here produces a
            // new membership, which is a different authorization than the one
            // that was confirmed.
            if !self.membership_unchanged(&plan.group_id, entry)? {
                outcomes.push(GroupOutcome {
                    session_id: entry.session_id,
                    result: GroupMemberResult::Skipped(GroupSkip::MembershipChanged),
                });
                continue;
            }
            let result = match plan.action {
                GroupAction::Stop => lifecycle
                    .stop_owned(entry.session_id)
                    .map(|closed| GroupMemberResult::Stopped(closed.workload)),
                GroupAction::Restart => lifecycle
                    .restart_owned(entry.session_id)
                    .map(|_| GroupMemberResult::Restarted),
            };
            outcomes.push(GroupOutcome {
                session_id: entry.session_id,
                result: result.unwrap_or_else(|error| {
                    GroupMemberResult::Failed(sanitize_group_failure(&error.to_string()))
                }),
            });
        }
        Ok(GroupActionReport {
            group_id: plan.group_id.clone(),
            action: plan.action,
            outcomes,
        })
    }

    fn membership_unchanged(
        &self,
        group_id: &OrchestrationGroupId,
        entry: &GroupPlanEntry,
    ) -> Result<bool> {
        Ok(self
            .store
            .load()?
            .orchestration_groups
            .iter()
            .find(|group| &group.id == group_id)
            .is_some_and(|group| {
                group.workers.iter().any(|member| {
                    member.session_id == entry.session_id
                        && member.membership_id == entry.membership_id
                })
            }))
    }
}

fn require_role_eligible_session(state: &State, session_id: SessionId, role: &str) -> Result<()> {
    let eligible = state.sessions.iter().any(|record| {
        record.id == session_id
            && record.status == SessionStatus::Running
            && record.ownership_proof.is_some()
            && record.tmux_session_id.is_some()
    });
    if !eligible {
        bail!("{role} is no longer a running exact-owned workload");
    }
    Ok(())
}
fn validate_worker_specs(
    orchestrator_session_id: SessionId,
    workers: &[OrchestrationWorkerSpec],
) -> Result<()> {
    if workers.len() > OrchestrationGroup::MAX_WORKERS {
        bail!(
            "orchestration group may contain at most {} workers",
            OrchestrationGroup::MAX_WORKERS
        );
    }
    let mut session_ids = HashSet::with_capacity(workers.len());
    for worker in workers {
        if worker.session_id == orchestrator_session_id {
            bail!("orchestrator must not also be a worker");
        }
        if !session_ids.insert(worker.session_id) {
            bail!("duplicate worker session `{}`", worker.session_id);
        }
        if worker.capabilities.is_empty() {
            bail!(
                "worker `{}` must declare at least one capability",
                worker.session_id
            );
        }
    }
    Ok(())
}

fn worker_members(
    existing: Vec<OrchestrationMember>,
    workers: Vec<OrchestrationWorkerSpec>,
) -> Vec<OrchestrationMember> {
    let existing = existing
        .into_iter()
        .map(|worker| (worker.session_id, worker))
        .collect::<HashMap<_, _>>();
    workers
        .into_iter()
        .map(|worker| {
            let membership_id = existing
                .get(&worker.session_id)
                .map_or_else(OrchestrationMembershipId::new, |member| {
                    member.membership_id
                });
            OrchestrationMember {
                session_id: worker.session_id,
                membership_id,
                title: worker.title,
                capabilities: worker.capabilities,
            }
        })
        .collect()
}

pub(crate) const fn companion_placement(placement: Placement) -> Placement {
    if matches!(placement, Placement::ReplaceCurrentPane) {
        Placement::SplitRight
    } else {
        placement
    }
}

const _: () = assert!(crate::observer::MAX_WORKERS == OrchestrationGroup::MAX_WORKERS);

const OBSERVER_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const OBSERVER_INPUT_POLL: Duration = Duration::from_millis(50);
const OBSERVER_OPEN_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Default)]
struct ObserverOpenGate {
    last_completed: Option<(String, Instant)>,
}

impl ObserverOpenGate {
    fn suppresses(&mut self, worker_id: &str, now: Instant) -> bool {
        let Some((previous_worker, completed_at)) = &mut self.last_completed else {
            return false;
        };
        if previous_worker != worker_id
            || now.saturating_duration_since(*completed_at) >= OBSERVER_OPEN_DEBOUNCE
        {
            return false;
        }
        *completed_at = now;
        true
    }

    fn record(&mut self, worker_id: &str, completed_at: Instant) {
        self.last_completed = Some((worker_id.to_owned(), completed_at));
    }
}

fn observer_key_for_event(key: KeyEvent) -> Option<(ObserverKey, ObserverInputKind)> {
    let kind = match key.kind {
        KeyEventKind::Press => ObserverInputKind::Press,
        KeyEventKind::Repeat => ObserverInputKind::Repeat,
        KeyEventKind::Release => return None,
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return Some((ObserverKey::ControlC, kind));
    }
    let key = match key.code {
        KeyCode::Up => ObserverKey::Up,
        KeyCode::Down => ObserverKey::Down,
        KeyCode::Left => ObserverKey::Left,
        KeyCode::Right => ObserverKey::Right,
        KeyCode::PageUp => ObserverKey::PageUp,
        KeyCode::PageDown => ObserverKey::PageDown,
        KeyCode::Tab => ObserverKey::Tab,
        KeyCode::BackTab => ObserverKey::BackTab,
        KeyCode::Enter => ObserverKey::Enter,
        KeyCode::Esc => ObserverKey::Escape,
        KeyCode::Char(character) => ObserverKey::Char(character),
        _ => return None,
    };
    Some((key, kind))
}

fn handle_observer_open(
    observer: &mut ObserverState,
    gate: &mut ObserverOpenGate,
    worker_id: &str,
    mut now: impl FnMut() -> Instant,
    mut render_feedback: impl FnMut(&ObserverState) -> Result<()>,
    mut open: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    if gate.suppresses(worker_id, now()) {
        let queued = "wait before retrying";
        let notice = observer.notice().map(str::to_owned);
        if let Some(failure) = notice.filter(|notice| notice.starts_with("Open failed:")) {
            if !failure.contains("queued Enter ignored") {
                observer.set_notice(Some(format!("{failure} · queued Enter ignored; {queued}")));
            }
        } else {
            observer.set_notice(Some(format!("Queued Enter ignored; {queued}")));
        }
        return Ok(());
    }

    observer.set_notice(Some("Opening selected worker…".to_owned()));
    render_feedback(observer)?;
    let result = open(worker_id);
    gate.record(worker_id, now());
    match result {
        Ok(()) => observer.set_notice(Some(
            "Worker opened; queued Enter will be ignored briefly".to_owned(),
        )),
        Err(error) => observer.set_notice(Some(format!("Open failed: {error:#}"))),
    }
    Ok(())
}

const MAX_REVIEWED_PROMPT_BYTES: usize = 16 * 1024;
fn read_bounded_prompt_line() -> Result<Option<String>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut bytes = Vec::with_capacity(MAX_REVIEWED_PROMPT_BYTES.min(1024));
    let mut byte = [0_u8; 1];
    let mut saw_input = false;
    let mut overflow = false;
    loop {
        let read = input
            .read(&mut byte)
            .context("read Mission Control prompt")?;
        if read == 0 {
            break;
        }
        saw_input = true;
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] == b'\r' {
            continue;
        }
        if bytes.len() < MAX_REVIEWED_PROMPT_BYTES {
            bytes.push(byte[0]);
        } else {
            overflow = true;
        }
    }
    if !saw_input {
        return Ok(None);
    }
    if overflow {
        anyhow::bail!("prompt exceeds the {MAX_REVIEWED_PROMPT_BYTES}-byte Mission Control limit");
    }
    String::from_utf8(bytes)
        .context("Mission Control prompt must be valid UTF-8")
        .map(Some)
}

fn review_mission_prompt(worker_ids: &[String]) -> Result<Option<String>> {
    execute!(io::stdout(), LeaveAlternateScreen).context("leave Mission Control screen")?;
    disable_raw_mode().context("suspend Mission Control raw mode")?;
    let input_result = (|| -> Result<Option<String>> {
        println!("Mission Control prompt");
        println!("Destinations:");
        for worker_id in worker_ids {
            println!("  - {worker_id}");
        }
        print!("\nPrompt: ");
        io::stdout().flush().context("flush prompt")?;
        let Some(prompt) = read_bounded_prompt_line()? else {
            return Ok(None);
        };
        if prompt.trim().is_empty() {
            return Ok(None);
        }
        println!("\nReview exact destinations:");
        for worker_id in worker_ids {
            println!("  - {worker_id}");
        }
        println!(
            "\nPrompt:\n{}\n",
            crate::observer::sanitize_capture(&prompt)
        );
        print!("Type SEND to deliver exactly once, or press Enter to cancel: ");
        io::stdout().flush().context("flush prompt review")?;
        let mut confirmation = String::new();
        io::stdin()
            .lock()
            .read_line(&mut confirmation)
            .context("read prompt confirmation")?;
        Ok((confirmation.trim() == "SEND").then_some(prompt))
    })();
    let raw_result = enable_raw_mode().context("resume Mission Control raw mode");
    let screen_result =
        execute!(io::stdout(), EnterAlternateScreen).context("resume Mission Control screen");
    raw_result?;
    screen_result?;
    input_result
}

fn mission_targets(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    worker_ids: &[String],
) -> Result<Vec<MemberTarget>> {
    let state = store
        .load_read_only()
        .context("load Mission Control destinations")?;
    let group = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
        .context("Mission Control group no longer exists")?;
    worker_ids
        .iter()
        .map(|worker_id| {
            let session_id = worker_id
                .parse::<SessionId>()
                .context("parse Mission Control worker ID")?;
            let member = group
                .workers
                .iter()
                .find(|member| member.session_id == session_id)
                .context("Mission Control worker membership changed")?;
            Ok(MemberTarget {
                session_id,
                membership_id: member.membership_id,
            })
        })
        .collect()
}

fn target_delivery_summary(deliveries: &[TargetDelivery]) -> String {
    deliveries
        .iter()
        .map(|delivery| match delivery {
            TargetDelivery::Delivered {
                session_id,
                final_state,
            } => format!(
                "{}:DELIVERED→{}",
                session_id.reference_token(SessionId::SHORT_REFERENCE_WIDTH),
                final_state.label()
            ),
            TargetDelivery::Rejected { session_id, .. } => format!(
                "{}:REJECTED",
                session_id.reference_token(SessionId::SHORT_REFERENCE_WIDTH)
            ),
            TargetDelivery::Stalled { session_id, .. } => format!(
                "{}:DELIVERED→NO CHANGE",
                session_id.reference_token(SessionId::SHORT_REFERENCE_WIDTH)
            ),
            TargetDelivery::Uncertain { session_id } => format!(
                "{}:UNCERTAIN",
                session_id.reference_token(SessionId::SHORT_REFERENCE_WIDTH)
            ),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn run_observer(
    paths: &AppPaths,
    group_id: OrchestrationGroupId,
    herdr_context: HerdrContext,
) -> Result<()> {
    let store = StateStore::new(paths.state_file.clone());
    let service = OrchestrationService::new(store.clone());
    let mission_client = HerdrSocketClient::from_env().ok();
    // Notifications are advisory, so an unreadable config must not stop the
    // Observer from opening; fall back to the documented defaults.
    let notifications = ConfigStore::new(paths.config_file.clone())
        .load()
        .map(|config| config.notifications)
        .unwrap_or_default();
    let group = service.group(&group_id)?;
    let mut observer = ObserverState::new(Vec::with_capacity(group.workers.len()));
    let mut capture_fingerprints = HashMap::new();
    let (capture_worker, observer_results) = CaptureWorker::spawn();
    // Herdr needs the pane ids up front, so the first refresh discovers the
    // group's panes and the event monitor subscribes afterwards.
    let mut agent_panes: Vec<String> = Vec::new();
    let (prompt_delivery_sender, prompt_delivery_results) = mpsc::channel::<Vec<TargetDelivery>>();
    let mut prompt_delivery: Option<JoinHandle<()>> = None;
    let mut force_quit_armed = false;
    let initial_refresh = refresh_observer_metadata(
        &store,
        &group_id,
        &mut observer,
        &mut capture_fingerprints,
        capture_worker.sender(),
        MissionRefresh {
            client: mission_client.as_ref(),
            notifications,
            agent_panes: &mut agent_panes,
        },
    )?;
    require_observer_authority(initial_refresh)?;
    let mut subscribed_panes = agent_panes.clone();
    let mut mission_events = mission_client
        .as_ref()
        .map(|client| client.subscribe(subscribed_panes.clone()));
    enable_raw_mode().context("enable Observer terminal raw mode")?;
    let _guard = ObserverTerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen).context("enter Observer alternate screen")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize Observer terminal")?;
    terminal.clear().context("clear Observer terminal")?;
    let mut last_refresh = Instant::now();
    let mut open_gate = ObserverOpenGate::default();

    loop {
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(frame, area, &observer);
            })
            .context("draw Observer")?;

        loop {
            match observer_results.try_recv() {
                Ok(result) if visible_worker_ids(&observer) == result.visible => {
                    let merge = merge_captured_workers(&store, &group_id, &mut observer, result);
                    let outcome = apply_observer_capture_merge_result(&mut observer, merge);
                    require_observer_authority(outcome)?;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    bail!("Observer capture worker stopped unexpectedly")
                }
            }
        }
        match prompt_delivery_results.try_recv() {
            Ok(deliveries) => {
                if let Some(handle) = prompt_delivery.take() {
                    let _ = handle.join();
                }
                force_quit_armed = false;
                observer.set_notice(Some(target_delivery_summary(&deliveries)));
            }
            Err(TryRecvError::Empty)
                if prompt_delivery
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished) =>
            {
                if let Some(handle) = prompt_delivery.take() {
                    let _ = handle.join();
                }
                force_quit_armed = false;
                observer.set_notice(Some(
                    "Prompt delivery worker ended without a result; outcome is UNCERTAIN"
                        .to_owned(),
                ));
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        let mut mission_changed = false;
        if let Some(events) = mission_events.as_ref() {
            while events.try_recv().is_ok() {
                mission_changed = true;
            }
        }
        if mission_changed {
            let refresh = refresh_observer_metadata(
                &store,
                &group_id,
                &mut observer,
                &mut capture_fingerprints,
                capture_worker.sender(),
                MissionRefresh {
                    client: mission_client.as_ref(),
                    notifications,
                    agent_panes: &mut agent_panes,
                },
            );
            let outcome = apply_observer_refresh_result(&mut observer, refresh);
            require_observer_authority(outcome)?;
            last_refresh = Instant::now();
        }
        if last_refresh.elapsed() >= OBSERVER_REFRESH_INTERVAL {
            let refresh = refresh_observer_metadata(
                &store,
                &group_id,
                &mut observer,
                &mut capture_fingerprints,
                capture_worker.sender(),
                MissionRefresh {
                    client: mission_client.as_ref(),
                    notifications,
                    agent_panes: &mut agent_panes,
                },
            );
            let outcome = apply_observer_refresh_result(&mut observer, refresh);
            require_observer_authority(outcome)?;
            last_refresh = Instant::now();
        }
        if agent_panes != subscribed_panes {
            // Members are opened and closed while Mission Control is running, so
            // the watched pane set has to follow. Herdr fixes a subscription's
            // panes at subscribe time, which means re-establishing it.
            subscribed_panes.clone_from(&agent_panes);
            mission_events = mission_client
                .as_ref()
                .map(|client| client.subscribe(subscribed_panes.clone()));
        }
        if !event::poll(OBSERVER_INPUT_POLL).context("poll Observer input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read Observer input")? else {
            continue;
        };
        let Some((observer_key, input_kind)) = observer_key_for_event(key) else {
            continue;
        };
        // Navigation and quit remain responsive during prompt-and-wait. Every
        // operation that could duplicate or retarget input is gated while busy.
        let prompt_busy = prompt_delivery.is_some();
        let Some(action) = observer.action_for_input(observer_key, input_kind, prompt_busy) else {
            continue;
        };
        if !matches!(action, ObserverAction::Quit) {
            force_quit_armed = false;
        }
        clear_transient_observer_notice(&mut observer, &ObserverOutcome::None);
        let previous_page = observer.page();
        let outcome = observer.apply(action);
        match outcome {
            ObserverOutcome::None if observer.page() != previous_page => {
                let refresh = refresh_observer_metadata(
                    &store,
                    &group_id,
                    &mut observer,
                    &mut capture_fingerprints,
                    capture_worker.sender(),
                    MissionRefresh {
                        client: mission_client.as_ref(),
                        notifications,
                        agent_panes: &mut agent_panes,
                    },
                );
                let outcome = apply_observer_refresh_result(&mut observer, refresh);
                require_observer_authority(outcome)?;
                last_refresh = Instant::now();
            }
            ObserverOutcome::None => {}
            ObserverOutcome::Refresh => {
                let refresh = refresh_observer_metadata(
                    &store,
                    &group_id,
                    &mut observer,
                    &mut capture_fingerprints,
                    capture_worker.sender(),
                    MissionRefresh {
                        client: mission_client.as_ref(),
                        notifications,
                        agent_panes: &mut agent_panes,
                    },
                );
                let outcome = apply_observer_refresh_result(&mut observer, refresh);
                require_observer_authority(outcome)?;
                last_refresh = Instant::now();
            }
            ObserverOutcome::OpenSelected { worker_id } => {
                handle_observer_open(
                    &mut observer,
                    &mut open_gate,
                    &worker_id,
                    Instant::now,
                    |state| {
                        terminal
                            .draw(|frame| render(frame, frame.area(), state))
                            .context("draw Observer open feedback")?;
                        Ok(())
                    },
                    |worker_id| {
                        open_worker(
                            paths,
                            &store,
                            &group_id,
                            worker_id,
                            &herdr_context,
                            mission_client.as_ref(),
                        )
                    },
                )?;
            }
            ObserverOutcome::OpenUnavailable { worker_id } => {
                observer.set_notice(Some(format!(
                    "Worker {worker_id} is not authorized, running, and exact-owned"
                )));
            }
            ObserverOutcome::ComposePrompt { worker_ids } => {
                let Some(client) = mission_client.as_ref() else {
                    observer.set_notice(Some(
                        "Mission Control agent actions require a reachable Herdr session"
                            .to_owned(),
                    ));
                    continue;
                };
                match mission_targets(&store, &group_id, &worker_ids) {
                    Ok(targets) => match review_mission_prompt(&worker_ids) {
                        Ok(Some(prompt)) => {
                            let mission = MissionControlService::new(store.clone(), client.clone());
                            let delivery_group = group_id.clone();
                            let delivery_sender = prompt_delivery_sender.clone();
                            let target_count = targets.len();
                            force_quit_armed = false;
                            prompt_delivery = Some(thread::spawn(move || {
                                let deliveries = mission.deliver_reviewed_prompt(
                                    &delivery_group,
                                    &targets,
                                    &prompt,
                                    true,
                                );
                                let _ = delivery_sender.send(deliveries);
                            }));
                            observer.set_notice(Some(format!(
                                "Prompt delivery in progress for {target_count} reviewed target(s)"
                            )));
                        }
                        Ok(None) => {
                            observer.set_notice(Some("Prompt cancelled; nothing sent".to_owned()));
                        }
                        Err(error) => {
                            observer.set_notice(Some(format!("Prompt review failed: {error:#}")));
                        }
                    },
                    Err(error) => {
                        observer.set_notice(Some(format!("Prompt targets changed: {error:#}")));
                    }
                }
                terminal
                    .clear()
                    .context("clear Mission Control after prompt review")?;
            }
            ObserverOutcome::FocusSelected { worker_id } => {
                let Some(client) = mission_client.as_ref() else {
                    observer.set_notice(Some(
                        "Mission Control agent actions require a reachable Herdr session"
                            .to_owned(),
                    ));
                    continue;
                };
                let result = mission_targets(&store, &group_id, std::slice::from_ref(&worker_id))
                    .and_then(|targets| {
                        let mission = MissionControlService::new(store.clone(), client.clone());
                        let binding = mission.binding_for_open(&group_id, &targets[0])?;
                        client.focus_agent(binding.target())
                    });
                observer.set_notice(Some(match result {
                    Ok(agent) => format!(
                        "Focused {} · {}",
                        worker_id,
                        MissionAgentState::from(agent.agent_status).label()
                    ),
                    Err(error) => format!("Focus rejected: {error:#}"),
                }));
            }
            ObserverOutcome::WaitSelected { worker_id } => {
                let Some(client) = mission_client.as_ref() else {
                    observer.set_notice(Some(
                        "Mission Control agent actions require a reachable Herdr session"
                            .to_owned(),
                    ));
                    continue;
                };
                let result = mission_targets(&store, &group_id, std::slice::from_ref(&worker_id))
                    .and_then(|targets| {
                        let mission = MissionControlService::new(store.clone(), client.clone());
                        let binding = mission.binding_for_observation(&group_id, &targets[0])?;
                        client.wait_agent(binding.target(), Duration::from_secs(2))
                    });
                observer.set_notice(Some(match result {
                    Ok(agent) => format!(
                        "Wait returned {} for {}",
                        MissionAgentState::from(agent.agent_status).label(),
                        worker_id
                    ),
                    Err(error) => format!("Wait ended without a state change: {error:#}"),
                }));
            }
            ObserverOutcome::ReadSelected { worker_id } => {
                let Some(client) = mission_client.as_ref() else {
                    observer.set_notice(Some(
                        "Mission Control agent actions require a reachable Herdr session"
                            .to_owned(),
                    ));
                    continue;
                };
                let result = mission_targets(&store, &group_id, std::slice::from_ref(&worker_id))
                    .and_then(|targets| {
                        let mission = MissionControlService::new(store.clone(), client.clone());
                        let binding = mission.binding_for_observation(&group_id, &targets[0])?;
                        client.agent_read(binding.target(), 80)
                    });
                match result {
                    Ok(read) => {
                        let truncated = read.truncated;
                        observer.merge_capture(
                            &worker_id,
                            if truncated {
                                ObserverCapture::Truncated(read.text)
                            } else {
                                ObserverCapture::Ready(read.text)
                            },
                        );
                        observer.set_notice(Some(if truncated {
                            format!(
                                "Read Herdr agent output for {worker_id} · older output was dropped"
                            )
                        } else {
                            format!("Read Herdr agent output for {worker_id}")
                        }));
                    }
                    Err(error) => {
                        observer.set_notice(Some(format!("Read rejected: {error:#}")));
                    }
                }
            }
            ObserverOutcome::ExplainSelected { worker_id } => {
                let Some(client) = mission_client.as_ref() else {
                    observer.set_notice(Some(
                        "Mission Control agent actions require a reachable Herdr session"
                            .to_owned(),
                    ));
                    continue;
                };
                let result = mission_targets(&store, &group_id, std::slice::from_ref(&worker_id))
                    .and_then(|targets| {
                        let mission = MissionControlService::new(store.clone(), client.clone());
                        let binding = mission.binding_for_observation(&group_id, &targets[0])?;
                        client.explain_agent(binding.target())
                    });
                match result {
                    Ok(fields) if fields.is_empty() => {
                        observer.set_notice(Some(format!(
                            "Herdr returned no explanation for {worker_id}"
                        )));
                    }
                    Ok(fields) => {
                        // Herdr owns the reasoning; Tether only relays it.
                        let summary = fields
                            .iter()
                            .map(|(key, value)| format!("{key}: {value}"))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        observer
                            .set_notice(Some(format!("Herdr explains {worker_id} — {summary}")));
                    }
                    Err(error) => {
                        observer.set_notice(Some(format!("Explain rejected: {error:#}")));
                    }
                }
            }
            ObserverOutcome::Quit if prompt_delivery.is_some() && force_quit_armed => {
                bail!(
                    "Mission Control closed while reviewed prompt delivery was still in flight; per-target outcome is UNCERTAIN"
                )
            }
            ObserverOutcome::Quit if prompt_delivery.is_some() => {
                force_quit_armed = true;
                observer.set_notice(Some(
                    "Prompt still in flight · wait for results or press q/Ctrl+C again to close as UNCERTAIN"
                        .to_owned(),
                ));
            }
            ObserverOutcome::Quit => return Ok(()),
        }
    }
}

fn clear_transient_observer_notice(observer: &mut ObserverState, outcome: &ObserverOutcome) {
    if !matches!(outcome, ObserverOutcome::OpenSelected { .. })
        && observer.notice() != Some(OBSERVER_REFRESH_FAILURE_NOTICE)
    {
        observer.set_notice(None);
    }
}

fn apply_observer_refresh_result(
    observer: &mut ObserverState,
    result: Result<ObserverAuthorityOutcome>,
) -> ObserverAuthorityOutcome {
    match result {
        Ok(ObserverAuthorityOutcome::Authorized) => {
            if observer.notice() == Some(OBSERVER_REFRESH_FAILURE_NOTICE) {
                observer.set_notice(None);
            }
            ObserverAuthorityOutcome::Authorized
        }
        Ok(ObserverAuthorityOutcome::GroupDeleted) => ObserverAuthorityOutcome::GroupDeleted,
        Ok(ObserverAuthorityOutcome::RecoverableFailure) | Err(_) => {
            observer.set_notice(Some(OBSERVER_REFRESH_FAILURE_NOTICE.to_owned()));
            ObserverAuthorityOutcome::RecoverableFailure
        }
    }
}

fn apply_observer_capture_merge_result(
    observer: &mut ObserverState,
    result: Result<ObserverAuthorityOutcome>,
) -> ObserverAuthorityOutcome {
    match result {
        Ok(outcome) => outcome,
        Err(_) => {
            observer.set_notice(Some(OBSERVER_REFRESH_FAILURE_NOTICE.to_owned()));
            ObserverAuthorityOutcome::RecoverableFailure
        }
    }
}

fn require_observer_authority(outcome: ObserverAuthorityOutcome) -> Result<()> {
    match outcome {
        ObserverAuthorityOutcome::Authorized | ObserverAuthorityOutcome::RecoverableFailure => {
            Ok(())
        }
        ObserverAuthorityOutcome::GroupDeleted => bail!(OBSERVER_GROUP_DELETED_ERROR),
    }
}

fn refresh_observer_metadata(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    observer: &mut ObserverState,
    capture_fingerprints: &mut HashMap<String, CaptureFingerprint>,
    capture_requests: &SyncSender<CaptureRequest>,
    mission: MissionRefresh<'_>,
) -> Result<ObserverAuthorityOutcome> {
    let MissionRefresh {
        client: mission_client,
        notifications,
        agent_panes,
    } = mission;
    let state = store.load()?;
    let Some(group) = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
    else {
        return Ok(ObserverAuthorityOutcome::GroupDeleted);
    };
    let mission_started = Instant::now();
    let (mission_snapshot, mission_unreachable, mission_latency_ms) =
        match mission_client.map(HerdrSocketClient::snapshot) {
            Some(Ok(snapshot)) if snapshot.supports_protocol() => (
                Some(snapshot),
                false,
                Some(u64::try_from(mission_started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            ),
            Some(Ok(_)) | None => (None, false, None),
            Some(Err(_)) => (None, true, None),
        };
    let next_fingerprints = capture_fingerprints_for(group, &state.sessions);
    let attention = update_observer_metadata(
        observer,
        capture_fingerprints,
        &next_fingerprints,
        observer_workers(
            group,
            &state,
            mission_snapshot.as_ref(),
            mission_unreachable,
            mission_latency_ms,
        ),
    );
    notify_attention(mission_client, notifications, &attention);
    *agent_panes = group_agent_panes(mission_snapshot.as_ref(), group_id);
    *capture_fingerprints = next_fingerprints;
    let request = CaptureRequest {
        group: group.clone(),
        sessions: state.sessions,
        visible: visible_worker_ids(observer),
    };
    match capture_requests.try_send(request) {
        Ok(()) | Err(TrySendError::Full(_)) => Ok(ObserverAuthorityOutcome::Authorized),
        Err(TrySendError::Disconnected(_)) => {
            bail!("Observer capture worker stopped unexpectedly")
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct CaptureFingerprint {
    membership_id: OrchestrationMembershipId,
    session_id: SessionId,
    ownership_proof: OwnershipProof,
    tmux_session_id: TmuxSessionId,
    capabilities: OrchestrationCapabilities,
}

struct CapturedWorker {
    fingerprint: CaptureFingerprint,
    capture: ObserverCapture,
    latency_ms: u64,
}

struct CaptureResult {
    visible: HashSet<String>,
    workers: Vec<CapturedWorker>,
}

struct CaptureWorker {
    sender: Option<SyncSender<CaptureRequest>>,
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl CaptureWorker {
    fn spawn() -> (Self, mpsc::Receiver<CaptureResult>) {
        let (sender, receiver) = mpsc::sync_channel::<CaptureRequest>(1);
        let (results, observer_results) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            while let Ok(request) = receiver.recv() {
                if worker_shutdown.load(Ordering::Acquire) {
                    break;
                }
                let workers = captured_workers(&request.group, &request.sessions, &request.visible);
                if results
                    .send(CaptureResult {
                        visible: request.visible,
                        workers,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        (
            Self {
                sender: Some(sender),
                handle: Some(handle),
                shutdown,
            },
            observer_results,
        )
    }

    fn sender(&self) -> &SyncSender<CaptureRequest> {
        self.sender.as_ref().expect("capture sender is live")
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn update_observer_metadata(
    observer: &mut ObserverState,
    previous_fingerprints: &HashMap<String, CaptureFingerprint>,
    current_fingerprints: &HashMap<String, CaptureFingerprint>,
    mut workers: Vec<ObserverWorker>,
) -> Vec<WorkerAttention> {
    let previous_captures: HashMap<_, _> = observer
        .workers()
        .iter()
        .filter_map(|worker| {
            worker
                .capture
                .as_ref()
                .map(|capture| (worker.id.clone(), capture.clone()))
        })
        .collect();
    for worker in &mut workers {
        let same_epoch = previous_fingerprints
            .get(&worker.id)
            .zip(current_fingerprints.get(&worker.id))
            .is_some_and(|(previous, current)| previous == current);
        if same_epoch {
            if let Some(capture) = previous_captures.get(&worker.id) {
                worker.capture = Some(capture.clone());
            }
        } else {
            observer.merge_capture(&worker.id, ObserverCapture::Loading);
        }
    }
    observer.update_workers(workers)
}

/// Mission Control inputs and outputs for one Observer refresh.
struct MissionRefresh<'a> {
    client: Option<&'a HerdrSocketClient>,
    notifications: NotificationDefaults,
    /// Receives the panes whose agent status this Observer should watch.
    agent_panes: &'a mut Vec<String>,
}

/// Lists the Herdr panes currently carrying this group's Tether token.
///
/// Herdr requires a `pane_id` on `pane.agent_status_changed` subscriptions, so
/// Mission Control has to name the panes it wants transitions for. The group
/// token is exactly that set, and it comes straight from the snapshot without
/// re-resolving bindings.
fn group_agent_panes(
    snapshot: Option<&HerdrSessionSnapshot>,
    group_id: &OrchestrationGroupId,
) -> Vec<String> {
    let group = group_id.to_string();
    let mut panes: Vec<String> = snapshot
        .map(|snapshot| {
            snapshot
                .agents
                .iter()
                .filter(|agent| agent.tokens.get(GROUP_TOKEN) == Some(&group))
                .map(|agent| agent.pane_id.clone())
                .collect()
        })
        .unwrap_or_default();
    panes.sort();
    panes.dedup();
    panes
}

/// Asks Herdr to show a toast for each newly attention-worthy worker.
///
/// Delivery is best effort: the Observer tile is the authoritative view, so a
/// disabled or refused toast changes nothing and is never surfaced as an error.
/// The body carries a workload reference and either a state or an exit status.
/// Host, directory, command, capture, and prompt text never travel, which is why
/// it is not the tile's display title: that title is generated from the host,
/// repository name, and preset.
fn notify_attention(
    mission_client: Option<&HerdrSocketClient>,
    notifications: NotificationDefaults,
    attention: &[WorkerAttention],
) {
    let Some(client) = mission_client else {
        return;
    };
    for event in attention {
        // The body names the workload rather than describing it. A display title
        // is generated from host, repository name, and preset, and a toast leaves
        // the surface that produced it, so the reference travels and the title
        // stays on the tile. The exit status is a small integer `tmux` reported,
        // not output, and it is the one detail that makes the toast actionable.
        let (enabled, sound, body) = match event.reason {
            AttentionReason::Agent(state @ ObserverAgentState::Blocked) => (
                notifications.agent_blocked,
                NotificationSound::Request,
                format!("Workload {} is {}", event.reference, state.label()),
            ),
            AttentionReason::Agent(state @ ObserverAgentState::Done) => (
                notifications.agent_done,
                NotificationSound::Done,
                format!("Workload {} is {}", event.reference, state.label()),
            ),
            AttentionReason::Agent(_) => continue,
            AttentionReason::Failed { exit_status } => (
                notifications.workload_failed,
                NotificationSound::Request,
                format!(
                    "Workload {} exited with status {exit_status}",
                    event.reference
                ),
            ),
        };
        if !enabled {
            continue;
        }
        let _ = client.show_notification("Tether", Some(&body), sound);
    }
}

fn merge_captured_workers(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    observer: &mut ObserverState,
    result: CaptureResult,
) -> Result<ObserverAuthorityOutcome> {
    let state = store.load()?;
    let Some(group) = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
    else {
        return Ok(ObserverAuthorityOutcome::GroupDeleted);
    };
    let current_fingerprints = capture_fingerprints_for(group, &state.sessions);
    for captured in result.workers {
        let id = captured.fingerprint.session_id.to_string();
        if !result.visible.contains(&id)
            || current_fingerprints.get(&id) != Some(&captured.fingerprint)
        {
            continue;
        }
        if !matches!(&captured.capture, ObserverCapture::Unavailable) {
            observer.set_connection_observation(
                &id,
                captured.latency_ms,
                Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
            );
        }
        observer.merge_capture(&id, captured.capture);
    }
    Ok(ObserverAuthorityOutcome::Authorized)
}

fn capture_fingerprints_for(
    group: &OrchestrationGroup,
    sessions: &[SessionRecord],
) -> HashMap<String, CaptureFingerprint> {
    let sessions_by_id: HashMap<_, _> = sessions.iter().map(|record| (record.id, record)).collect();
    group
        .workers
        .iter()
        .filter(|member| member.capabilities.observe_output)
        .filter_map(|member| {
            let record = sessions_by_id.get(&member.session_id)?;
            capture_fingerprint(member, record)
                .map(|fingerprint| (member.session_id.to_string(), fingerprint))
        })
        .collect()
}

fn capture_fingerprint(
    member: &OrchestrationMember,
    record: &SessionRecord,
) -> Option<CaptureFingerprint> {
    if record.status != SessionStatus::Running {
        return None;
    }
    Some(CaptureFingerprint {
        membership_id: member.membership_id,
        session_id: member.session_id,
        ownership_proof: record.ownership_proof?,
        tmux_session_id: record.tmux_session_id?,
        capabilities: member.capabilities,
    })
}

struct CaptureRequest {
    group: OrchestrationGroup,
    sessions: Vec<SessionRecord>,
    visible: HashSet<String>,
}

fn visible_worker_ids(observer: &ObserverState) -> HashSet<String> {
    observer
        .visible_workers()
        .iter()
        .filter(|worker| !worker.uses_live_agent())
        .map(|worker| worker.id.clone())
        .collect()
}

fn observer_workers(
    group: &OrchestrationGroup,
    state: &State,
    snapshot: Option<&HerdrSessionSnapshot>,
    mission_unreachable: bool,
    mission_latency_ms: Option<u64>,
) -> Vec<ObserverWorker> {
    let sessions_by_id: HashMap<_, _> = state
        .sessions
        .iter()
        .map(|record| (record.id, record))
        .collect();
    group
        .workers
        .iter()
        .map(|member| {
            let record = sessions_by_id.get(&member.session_id).copied();
            let owned = record.is_some_and(|record| {
                record.ownership_proof.is_some() && record.tmux_session_id.is_some()
            });
            let lifecycle = match record.map(|record| record.status) {
                Some(SessionStatus::Creating) => ObserverLifecycle::Starting,
                Some(SessionStatus::Running) => ObserverLifecycle::Running,
                Some(SessionStatus::Stopping) => ObserverLifecycle::Stopping,
                Some(SessionStatus::Ended) => match record.and_then(|record| record.exit_status) {
                    // A status `tmux` could not report is an unknown outcome,
                    // and an unknown outcome is not a failure.
                    Some(exit_status) if exit_status != 0 => {
                        ObserverLifecycle::Failed { exit_status }
                    }
                    _ => ObserverLifecycle::Ended,
                },
                Some(SessionStatus::Removed) => ObserverLifecycle::Removed,
                None => ObserverLifecycle::Missing,
            };
            let (agent_state, last_observed, live_agent) = if let Some(snapshot) = snapshot {
                let target = MemberTarget {
                    session_id: member.session_id,
                    membership_id: member.membership_id,
                };
                match crate::mission_control::binding_expectation(state, &group.id, &target) {
                    Ok(expectation) => match resolve_binding(snapshot, &expectation) {
                        Ok(binding) => (
                            observer_agent_state(binding.status.into()),
                            Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
                            true,
                        ),
                        Err(failure) => (observer_agent_state(failure.state()), None, false),
                    },
                    Err(_) => (ObserverAgentState::Unknown, None, false),
                }
            } else if mission_unreachable
                && record.is_some_and(|record| record.herdr_agent.is_some())
            {
                (ObserverAgentState::Unreachable, None, false)
            } else {
                (ObserverAgentState::Detached, None, false)
            };
            ObserverWorker {
                id: member.session_id.to_string(),
                title: member.title.as_ref().map(|title| title.as_str().to_owned()),
                capabilities: ObserverCapabilities {
                    observe_output: member.capabilities.observe_output,
                    open_interactive: member.capabilities.open_interactive,
                    prompt_agent: member.capabilities.prompt_agent,
                },
                lifecycle,
                agent_state,
                live_agent,
                owned,
                last_observed,
                incarnation: record.and_then(|record| record.tmux_session_id),
                latency_ms: live_agent.then_some(mission_latency_ms).flatten(),
                capture: None,
                // In this projection the only way to be stale is a binding that
                // is no longer exactly one recognized occupant; a lost
                // connection reads as unreachable here and is named later.
                stale_reason: (agent_state == ObserverAgentState::Stale)
                    .then_some(StaleReason::Binding),
            }
        })
        .collect()
}

const fn observer_agent_state(state: MissionAgentState) -> ObserverAgentState {
    match state {
        MissionAgentState::Detached => ObserverAgentState::Detached,
        MissionAgentState::Idle => ObserverAgentState::Idle,
        MissionAgentState::Working => ObserverAgentState::Working,
        MissionAgentState::Blocked => ObserverAgentState::Blocked,
        MissionAgentState::Done => ObserverAgentState::Done,
        MissionAgentState::Unknown => ObserverAgentState::Unknown,
        MissionAgentState::Unreachable => ObserverAgentState::Unreachable,
        MissionAgentState::Stale => ObserverAgentState::Stale,
    }
}

fn captured_workers(
    group: &OrchestrationGroup,
    sessions: &[SessionRecord],
    visible: &HashSet<String>,
) -> Vec<CapturedWorker> {
    let sessions_by_id: HashMap<_, _> = sessions.iter().map(|record| (record.id, record)).collect();
    thread::scope(|scope| {
        let handles = group
            .workers
            .iter()
            .filter_map(|member| {
                let id = member.session_id.to_string();
                let record = sessions_by_id.get(&member.session_id).copied()?;
                let fingerprint = capture_fingerprint(member, record)?;
                (visible.contains(&id) && member.capabilities.observe_output).then(|| {
                    (
                        fingerprint,
                        scope.spawn(move || {
                            let started = Instant::now();
                            let capture = capture_record(record);
                            let latency_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            (capture, latency_ms)
                        }),
                    )
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(fingerprint, handle)| {
                let (capture, latency_ms) =
                    handle.join().unwrap_or((ObserverCapture::Unavailable, 0));
                CapturedWorker {
                    fingerprint,
                    capture,
                    latency_ms,
                }
            })
            .collect()
    })
}

fn capture_record(record: &SessionRecord) -> ObserverCapture {
    let (Some(proof), Some(identity)) = (&record.ownership_proof, record.tmux_session_id) else {
        return ObserverCapture::Unavailable;
    };
    let Ok(backend) = backend_for_target(&record.target) else {
        return ObserverCapture::Unavailable;
    };
    match backend.capture_owned(&record.id, proof, identity) {
        Ok(capture) => ObserverCapture::Ready(capture.into_text()),
        Err(_) => ObserverCapture::Unavailable,
    }
}

fn open_worker(
    paths: &AppPaths,
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    worker_id: &str,
    herdr_context: &HerdrContext,
    mission_client: Option<&HerdrSocketClient>,
) -> Result<()> {
    let state = store.load()?;
    let group = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
        .with_context(|| format!("orchestration group `{group_id}` was deleted"))?;
    let member = group
        .workers
        .iter()
        .find(|member| member.session_id.to_string() == worker_id)
        .with_context(|| format!("worker `{worker_id}` is no longer a member of `{group_id}`"))?;
    if !member.capabilities.open_interactive {
        bail!("worker `{worker_id}` does not allow interactive open");
    }
    let record = state
        .sessions
        .iter()
        .find(|record| record.id == member.session_id)
        .with_context(|| format!("worker session `{worker_id}` is missing"))?;
    if record.status != SessionStatus::Running
        || record.ownership_proof.is_none()
        || record.tmux_session_id.is_none()
    {
        bail!("worker session `{worker_id}` is not a running exact-owned session");
    }
    let lifecycle = LifecycleService::new(store.clone(), ProcessBinaries::new("ssh", "tmux"));
    let command = lifecycle.open_owned(record.id)?;
    let title = PaneTitle::owned(
        &record.host,
        &record.directory,
        record.preset.as_deref(),
        record.command.as_deref(),
    );
    let placement = companion_placement(
        ConfigStore::new(paths.config_file.clone())
            .load()?
            .ui
            .placement,
    );
    let placed = HerdrClient::new(herdr_context.clone()).place(&command, &title, placement)?;
    if let Some(client) = mission_client {
        label_materialized_member(
            client,
            &placed.pane_id,
            group_id,
            member,
            record.target != "local",
        )
        .with_context(|| {
            format!(
                "opened worker `{worker_id}` in pane `{}` but could not bind it to Mission Control",
                placed.pane_id
            )
        })?;
    }
    Ok(())
}

fn backend_for_target(target: &str) -> Result<TmuxBackend> {
    let binaries = ProcessBinaries::new("ssh", "tmux");
    if target == "local" {
        Ok(TmuxBackend::local(binaries))
    } else {
        TmuxBackend::remote(target.to_owned(), binaries)
    }
}

trait TerminalCleanup {
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

struct CrosstermCleanup;

impl TerminalCleanup for CrosstermCleanup {
    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

fn restore_terminal(cleanup: &mut impl TerminalCleanup) {
    let _ = cleanup.show_cursor();
    let _ = cleanup.leave_alternate_screen();
    let _ = cleanup.disable_raw_mode();
}

struct ObserverTerminalGuard;

impl Drop for ObserverTerminalGuard {
    fn drop(&mut self) {
        restore_terminal(&mut CrosstermCleanup);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::observer::{ObserverAction, action_for_input};

    #[test]
    fn prompt_delivery_summary_keeps_safety_distinctions_visible() {
        let delivered: SessionId = "tether-0197f198000070008000000000000081".parse().unwrap();
        let rejected: SessionId = "tether-0197f198000070008000000000000082".parse().unwrap();
        let stalled: SessionId = "tether-0197f198000070008000000000000083".parse().unwrap();
        let uncertain: SessionId = "tether-0197f198000070008000000000000084".parse().unwrap();
        let summary = target_delivery_summary(&[
            TargetDelivery::Delivered {
                session_id: delivered,
                final_state: MissionAgentState::Done,
            },
            TargetDelivery::Rejected {
                session_id: rejected,
                reason: "fixture".to_owned(),
            },
            TargetDelivery::Stalled {
                session_id: stalled,
                reason: "fixture".to_owned(),
            },
            TargetDelivery::Uncertain {
                session_id: uncertain,
            },
        ]);

        assert_eq!(
            summary,
            format!(
                "{}:DELIVERED→DONE · {}:REJECTED · {}:DELIVERED→NO CHANGE · {}:UNCERTAIN",
                delivered.reference_token(SessionId::SHORT_REFERENCE_WIDTH),
                rejected.reference_token(SessionId::SHORT_REFERENCE_WIDTH),
                stalled.reference_token(SessionId::SHORT_REFERENCE_WIDTH),
                uncertain.reference_token(SessionId::SHORT_REFERENCE_WIDTH),
            )
        );
    }

    fn worker(id: &str, lifecycle: ObserverLifecycle, capture: Option<&str>) -> ObserverWorker {
        ObserverWorker {
            id: id.to_owned(),
            title: Some(format!("Worker {id}")),
            capabilities: ObserverCapabilities {
                observe_output: true,
                open_interactive: true,
                prompt_agent: false,
            },
            lifecycle,
            agent_state: ObserverAgentState::Detached,
            live_agent: false,
            owned: true,
            last_observed: None,
            incarnation: None,
            latency_ms: None,
            capture: capture.map(str::to_owned),
            stale_reason: None,
        }
    }

    fn exact_running_session(id: SessionId, tmux_id: &str) -> SessionRecord {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        SessionRecord {
            herdr_agent: None,
            id,
            host: "local".to_owned(),
            target: "local".to_owned(),
            directory: "/tmp".to_owned(),
            preset: None,
            command: Some("exec true".to_owned()),
            tmux_session_id: Some(tmux_id.parse().unwrap()),
            ownership_proof: Some(
                format!(
                    "0197f1980000700080000000000000{:0>2}",
                    tmux_id.trim_start_matches('$')
                )
                .parse()
                .unwrap(),
            ),
            status: SessionStatus::Running,
            created_at: now,
            last_used_at: now,
            closed_at: None,
            exit_status: None,
        }
    }

    #[test]
    fn attached_read_only_agents_do_not_spawn_recurring_tmux_capture_work() {
        let mut attached = worker("attached", ObserverLifecycle::Running, None);
        attached.live_agent = true;
        attached.agent_state = ObserverAgentState::Working;
        let mut detached = worker("detached", ObserverLifecycle::Running, None);
        detached.capabilities.prompt_agent = true;
        detached.agent_state = ObserverAgentState::Detached;
        let observer = ObserverState::new(vec![attached, detached]);

        assert_eq!(
            visible_worker_ids(&observer),
            HashSet::from(["detached".to_owned()])
        );
    }

    #[test]
    fn mixed_local_and_remote_agents_share_the_live_control_room() {
        use crate::{
            agent_view::GROUP_TOKEN,
            herdr_socket::{AgentStatus, HerdrAgentInfo},
            mission_control::{MEMBERSHIP_TOKEN, SESSION_TOKEN},
        };

        let local_id: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let remote_id: SessionId = "tether-0197f198000070008000000000000003".parse().unwrap();
        let local_membership: OrchestrationMembershipId =
            "0197f198000070008000000000000011".parse().unwrap();
        let remote_membership: OrchestrationMembershipId =
            "0197f198000070008000000000000012".parse().unwrap();
        let mut local = exact_running_session(local_id, "$1");
        local.herdr_agent = Some("codex".parse().unwrap());
        let mut remote = exact_running_session(remote_id, "$2");
        remote.target = "ssh:prod".to_owned();
        remote.host = "prod".to_owned();
        remote.herdr_agent = Some("codex".parse().unwrap());
        let group = OrchestrationGroup {
            id: "mixed-control".parse().unwrap(),
            title: "Mixed control".parse().unwrap(),
            orchestrator_session_id: "tether-0197f198000070008000000000000001".parse().unwrap(),
            workers: vec![
                OrchestrationMember {
                    session_id: local_id,
                    membership_id: local_membership,
                    title: Some("Local".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: true,
                        prompt_agent: true,
                    },
                },
                OrchestrationMember {
                    session_id: remote_id,
                    membership_id: remote_membership,
                    title: Some("Remote".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: true,
                        prompt_agent: false,
                    },
                },
            ],
        };
        let state = State {
            version: State::CURRENT_VERSION,
            sessions: vec![local, remote],
            orchestration_groups: vec![group],
        };
        let agent = |session_id: SessionId,
                     membership_id: OrchestrationMembershipId,
                     pane_id: &str,
                     status: AgentStatus| HerdrAgentInfo {
            terminal_id: format!("term-{pane_id}"),
            name: Some(format!("agent-{pane_id}")),
            agent: Some("codex".to_owned()),
            title: None,
            agent_status: status,
            tokens: HashMap::from([
                (GROUP_TOKEN.to_owned(), "mixed-control".to_owned()),
                (SESSION_TOKEN.to_owned(), session_id.to_string()),
                (MEMBERSHIP_TOKEN.to_owned(), membership_id.to_string()),
            ]),
            workspace_id: "w1".to_owned(),
            tab_id: "w1:t1".to_owned(),
            pane_id: pane_id.to_owned(),
            focused: false,
            state_change_seq: 1,
            revision: 1,
        };
        let snapshot = HerdrSessionSnapshot {
            version: "0.8.0".to_owned(),
            protocol: 19,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            panes: Vec::new(),
            agents: vec![
                agent(local_id, local_membership, "w1:p1", AgentStatus::Idle),
                agent(remote_id, remote_membership, "w1:p2", AgentStatus::Working),
            ],
        };

        let workers = observer_workers(
            &state.orchestration_groups[0],
            &state,
            Some(&snapshot),
            false,
            Some(17),
        );
        assert_eq!(workers[0].agent_state, ObserverAgentState::Idle);
        assert_eq!(workers[1].agent_state, ObserverAgentState::Working);
        assert!(workers.iter().all(ObserverWorker::uses_live_agent));
        assert!(workers.iter().all(|worker| worker.latency_ms == Some(17)));
        assert!(!workers[1].capabilities.prompt_agent);
        assert!(workers[1].can_observe_agent());
        assert!(!workers[1].can_prompt());
        let mut transitioned = snapshot.clone();
        for (status, expected) in [
            (AgentStatus::Working, ObserverAgentState::Working),
            (AgentStatus::Done, ObserverAgentState::Done),
        ] {
            transitioned.agents[0].agent_status = status;
            let refreshed = observer_workers(
                &state.orchestration_groups[0],
                &state,
                Some(&transitioned),
                false,
                Some(17),
            );
            assert_eq!(refreshed[0].agent_state, expected);
            assert!(refreshed[0].uses_live_agent());
        }
    }

    #[test]
    fn a_failing_exit_becomes_its_own_tile_state_and_an_unknown_one_does_not() {
        let worker_id = "tether-0197f198000070008000000000000041"
            .parse::<SessionId>()
            .unwrap();
        let membership_id: OrchestrationMembershipId =
            "0197f198000070008000000000000042".parse().unwrap();
        let group = OrchestrationGroup {
            id: "exit-control".parse().unwrap(),
            title: "Exit control".parse().unwrap(),
            orchestrator_session_id: worker_id,
            workers: vec![OrchestrationMember {
                session_id: worker_id,
                membership_id,
                title: Some("Worker".parse().unwrap()),
                capabilities: OrchestrationCapabilities {
                    observe_output: true,
                    open_interactive: true,
                    prompt_agent: false,
                },
            }],
        };
        let lifecycle_for = |status: SessionStatus, exit_status: Option<i32>| {
            let mut record = exact_running_session(worker_id, "$1");
            record.status = status;
            record.exit_status = exit_status;
            if status != SessionStatus::Running {
                record.closed_at = Some(record.last_used_at);
            }
            let state = State {
                version: State::CURRENT_VERSION,
                sessions: vec![record],
                orchestration_groups: vec![group.clone()],
            };
            observer_workers(&state.orchestration_groups[0], &state, None, false, None)[0].lifecycle
        };

        assert_eq!(
            lifecycle_for(SessionStatus::Ended, Some(2)),
            ObserverLifecycle::Failed { exit_status: 2 }
        );
        assert_eq!(
            lifecycle_for(SessionStatus::Ended, Some(0)),
            ObserverLifecycle::Ended,
            "a clean exit is not a failure"
        );
        // An explicit stop records no status, and `tmux` cannot always report
        // one. Neither is evidence of failure.
        assert_eq!(
            lifecycle_for(SessionStatus::Ended, None),
            ObserverLifecycle::Ended,
            "an unknown outcome is not a failure"
        );
        assert_eq!(
            lifecycle_for(SessionStatus::Running, None),
            ObserverLifecycle::Running
        );
    }

    #[cfg(unix)]
    fn served_notification(
        directory: &std::path::Path,
        name: &str,
        notifications: NotificationDefaults,
        attention: WorkerAttention,
    ) -> Option<serde_json::Value> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let socket = directory.join(name);
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            // Long enough for a local socket connection, short enough that the
            // cases which must send nothing do not pad the suite.
            for _ in 0..100 {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut request = String::new();
                        BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut request)
                            .unwrap();
                        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
                        let id = request["id"].as_str().unwrap().to_owned();
                        writeln!(
                            stream,
                            "{}",
                            serde_json::json!({
                                "id": id,
                                "result": {"type": "notification_show", "shown": true},
                            })
                        )
                        .unwrap();
                        return Some(request);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            None
        });
        notify_attention(
            Some(&HerdrSocketClient::new(socket)),
            notifications,
            &[attention],
        );
        server.join().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn each_attention_reason_notifies_with_its_own_sound_and_wording() {
        let temp = tempfile::tempdir().unwrap();
        let attention = |reason| WorkerAttention {
            worker_id: "tether-0197f198000070008000000000000001".to_owned(),
            reference: "…00000001".to_owned(),
            reason,
        };
        let cases = [
            (
                "blocked",
                AttentionReason::Agent(ObserverAgentState::Blocked),
                "request",
                "Workload …00000001 is BLOCKED",
            ),
            (
                "done",
                AttentionReason::Agent(ObserverAgentState::Done),
                "done",
                "Workload …00000001 is DONE",
            ),
            (
                "failed",
                AttentionReason::Failed { exit_status: 2 },
                "request",
                "Workload …00000001 exited with status 2",
            ),
        ];
        for (name, reason, sound, body) in cases {
            let request = served_notification(
                temp.path(),
                name,
                NotificationDefaults::default(),
                attention(reason),
            )
            .unwrap_or_else(|| panic!("{name} should have notified"));
            assert_eq!(request["method"], "notification.show");
            assert_eq!(request["params"]["title"], "Tether");
            assert_eq!(request["params"]["sound"], sound, "{name}");
            assert_eq!(request["params"]["body"], body, "{name}");
        }

        // A state nobody asked to hear about sends nothing at all.
        assert!(
            served_notification(
                temp.path(),
                "working",
                NotificationDefaults::default(),
                attention(AttentionReason::Agent(ObserverAgentState::Working)),
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_disabled_setting_sends_nothing_rather_than_a_suppressed_toast() {
        let temp = tempfile::tempdir().unwrap();
        let cases = [
            (
                "failed-off",
                NotificationDefaults {
                    workload_failed: false,
                    ..NotificationDefaults::default()
                },
                AttentionReason::Failed { exit_status: 2 },
            ),
            (
                "blocked-off",
                NotificationDefaults {
                    agent_blocked: false,
                    ..NotificationDefaults::default()
                },
                AttentionReason::Agent(ObserverAgentState::Blocked),
            ),
            (
                "done-off",
                NotificationDefaults {
                    agent_done: false,
                    ..NotificationDefaults::default()
                },
                AttentionReason::Agent(ObserverAgentState::Done),
            ),
        ];
        for (name, notifications, reason) in cases {
            assert!(
                served_notification(
                    temp.path(),
                    name,
                    notifications,
                    WorkerAttention {
                        worker_id: "w".to_owned(),
                        reference: "…00000001".to_owned(),
                        reason,
                    },
                )
                .is_none(),
                "{name} must not reach the socket"
            );
        }
    }

    #[derive(Default)]
    struct MockCleanup {
        attempts: Vec<&'static str>,
        fail: HashSet<&'static str>,
    }

    impl TerminalCleanup for MockCleanup {
        fn show_cursor(&mut self) -> io::Result<()> {
            self.attempts.push("show");
            if self.fail.contains("show") {
                return Err(io::Error::other("show failed"));
            }
            Ok(())
        }

        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.attempts.push("leave");
            if self.fail.contains("leave") {
                return Err(io::Error::other("leave failed"));
            }
            Ok(())
        }

        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.attempts.push("raw");
            if self.fail.contains("raw") {
                return Err(io::Error::other("raw failed"));
            }
            Ok(())
        }
    }

    #[test]
    fn terminal_cleanup_attempts_every_restoration_on_normal_exit() {
        let mut cleanup = MockCleanup::default();
        restore_terminal(&mut cleanup);
        assert_eq!(cleanup.attempts, ["show", "leave", "raw"]);
    }

    #[test]
    fn terminal_cleanup_attempts_every_restoration_after_errors() {
        let mut cleanup = MockCleanup {
            fail: HashSet::from(["show", "leave", "raw"]),
            ..MockCleanup::default()
        };
        restore_terminal(&mut cleanup);
        assert_eq!(cleanup.attempts, ["show", "leave", "raw"]);
    }

    #[test]
    fn queued_enter_events_place_the_selected_worker_once_until_input_is_quiet() {
        use std::cell::{Cell, RefCell};

        let start = Instant::now();
        let now = Cell::new(start);
        let placements = Cell::new(0usize);
        let feedback = RefCell::new(Vec::new());
        let mut gate = ObserverOpenGate::default();
        let mut observer = ObserverState::new(vec![worker(
            "selected",
            ObserverLifecycle::Running,
            Some("output"),
        )]);

        macro_rules! press_enter {
            () => {{
                let key = crossterm::event::KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    KeyEventKind::Press,
                );
                let (observer_key, input_kind) =
                    observer_key_for_event(key).expect("Enter press is handled");
                let action =
                    action_for_input(observer_key, input_kind, false).expect("Enter maps once");
                let ObserverOutcome::OpenSelected { worker_id } = observer.apply(action) else {
                    panic!("eligible Enter must request an open");
                };
                handle_observer_open(
                    &mut observer,
                    &mut gate,
                    &worker_id,
                    || now.get(),
                    |state| {
                        feedback
                            .borrow_mut()
                            .push(state.notice().unwrap_or_default().to_owned());
                        Ok(())
                    },
                    |_| {
                        placements.set(placements.get() + 1);
                        Ok(())
                    },
                )
                .unwrap();
            }};
        }

        press_enter!();
        now.set(start + Duration::from_millis(25));
        press_enter!();
        now.set(start + Duration::from_millis(50));
        press_enter!();

        assert_eq!(placements.get(), 1, "queued Enter must not fan out panes");
        assert!(
            feedback
                .borrow()
                .iter()
                .any(|notice| notice.contains("Opening"))
        );
        assert_eq!(
            observer.notice(),
            Some("Queued Enter ignored; wait before retrying")
        );
        now.set(start + OBSERVER_OPEN_DEBOUNCE + Duration::from_millis(20));
        press_enter!();
        assert_eq!(
            placements.get(),
            1,
            "suppressed input must extend the quiet window"
        );

        now.set(start + OBSERVER_OPEN_DEBOUNCE * 2 + Duration::from_millis(21));
        press_enter!();
        assert_eq!(
            placements.get(),
            2,
            "a later intentional gesture remains available"
        );
    }

    #[test]
    fn queued_enter_preserves_open_failure_feedback() {
        let start = Instant::now();
        let mut gate = ObserverOpenGate::default();
        let mut observer = ObserverState::new(vec![worker(
            "selected",
            ObserverLifecycle::Running,
            Some("output"),
        )]);

        handle_observer_open(
            &mut observer,
            &mut gate,
            "selected",
            || start,
            |_| Ok(()),
            |_| bail!("destination unavailable"),
        )
        .unwrap();
        assert!(
            observer
                .notice()
                .is_some_and(|notice| notice.contains("destination unavailable"))
        );

        handle_observer_open(
            &mut observer,
            &mut gate,
            "selected",
            || start + Duration::from_millis(25),
            |_| Ok(()),
            |_| panic!("queued Enter must not place again"),
        )
        .unwrap();
        let notice = observer.notice().unwrap();
        assert!(notice.contains("destination unavailable"), "{notice}");
        assert!(notice.contains("ignored"), "{notice}");
    }

    #[test]
    fn observer_input_repeats_navigation_but_not_single_shot_actions() {
        let repeat_navigation =
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat);
        let (key, kind) = observer_key_for_event(repeat_navigation).unwrap();
        assert_eq!(
            action_for_input(key, kind, false),
            Some(ObserverAction::NextWorker)
        );

        let repeat_open =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat);
        let (key, kind) = observer_key_for_event(repeat_open).unwrap();
        assert_eq!(action_for_input(key, kind, false), None);

        let release =
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(observer_key_for_event(release), None);
    }

    #[test]
    fn refresh_failure_retains_stale_tiles_and_context_until_retry_succeeds() {
        let mut observer = ObserverState::new(
            (0..5)
                .map(|index| {
                    worker(
                        &index.to_string(),
                        ObserverLifecycle::Running,
                        Some(&format!("first-{index}")),
                    )
                })
                .collect(),
        );
        for _ in 0..4 {
            observer.apply(ObserverAction::NextWorker);
        }
        let selected = observer.selected_id().unwrap().to_owned();
        let page = observer.page();
        let stale = observer.workers().to_vec();
        let mut refreshes = VecDeque::from([
            Ok(stale.clone()),
            Err(anyhow::anyhow!("metadata backend unavailable")),
            Ok(stale
                .iter()
                .cloned()
                .map(|mut worker| {
                    worker.capture = Some(format!("retry-{}", worker.id));
                    worker
                })
                .collect()),
        ]);

        for expected_success in [true, false, true] {
            let result = refreshes.pop_front().unwrap().map(|workers| {
                observer.update_workers(workers);
            });
            let expected_outcome = if expected_success {
                ObserverAuthorityOutcome::Authorized
            } else {
                ObserverAuthorityOutcome::RecoverableFailure
            };
            assert_eq!(
                apply_observer_refresh_result(
                    &mut observer,
                    result.map(|()| { ObserverAuthorityOutcome::Authorized })
                ),
                expected_outcome
            );
            assert_eq!(observer.selected_id(), Some(selected.as_str()));
            assert_eq!(observer.page(), page);
            if !expected_success {
                assert_eq!(observer.workers(), stale);
                let rendered = crate::observer::render_to_text(100, 14, &observer).unwrap();
                assert!(rendered.contains("first-4"), "{rendered}");
                assert!(rendered.contains("stale output retained"), "{rendered}");
                assert!(rendered.contains("r retry"), "{rendered}");
                assert!(rendered.contains("q back"), "{rendered}");
            }
        }
        assert!(observer.notice().is_none());
        assert_eq!(
            observer
                .selected_worker()
                .and_then(|worker| worker.capture.as_deref()),
            Some("retry-4")
        );
    }

    #[test]
    fn post_initial_capture_merge_load_failure_retains_stale_authority() {
        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let store = StateStore::new(state_path.clone());
        let (state, group_id, _) = capture_state("0197f198000070008000000000000011");
        let fingerprint = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(
            (0..5)
                .map(|index| {
                    worker(
                        &index.to_string(),
                        ObserverLifecycle::Running,
                        Some(&format!("stale-{index}")),
                    )
                })
                .collect(),
        );
        for _ in 0..4 {
            observer.apply(ObserverAction::NextWorker);
        }
        let stale = observer.workers().to_vec();
        let selected = observer.selected_id().unwrap().to_owned();
        let page = observer.page();
        std::fs::write(&state_path, "raw-store-error /private/state.json").unwrap();

        let result = merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(fingerprint, "new"),
        );
        assert_eq!(
            apply_observer_capture_merge_result(&mut observer, result),
            ObserverAuthorityOutcome::RecoverableFailure
        );

        assert_eq!(observer.workers(), stale);
        assert_eq!(observer.selected_id(), Some(selected.as_str()));
        assert_eq!(observer.page(), page);
        let rendered = crate::observer::render_to_text(100, 14, &observer).unwrap();
        assert!(rendered.contains("stale-4"), "{rendered}");
        assert!(rendered.contains("r retry"), "{rendered}");
        assert!(rendered.contains("q back"), "{rendered}");
        assert!(!rendered.contains("raw-store-error"), "{rendered}");

        assert!(!rendered.contains("/private/state.json"), "{rendered}");
    }

    #[test]
    fn authoritative_refresh_reports_group_deleted_without_replacing_stale_context() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let (mut state, group_id, _) = capture_state("0197f198000070008000000000000011");
        state.orchestration_groups.clear();
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            "selected",
            ObserverLifecycle::Running,
            Some("stale"),
        )]);
        let stale = observer.workers().to_vec();
        let mut fingerprints = HashMap::new();
        let (capture_requests, _) = mpsc::sync_channel(1);

        let outcome = refresh_observer_metadata(
            &store,
            &group_id,
            &mut observer,
            &mut fingerprints,
            &capture_requests,
            MissionRefresh {
                client: None,
                notifications: NotificationDefaults::default(),
                agent_panes: &mut Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(outcome, ObserverAuthorityOutcome::GroupDeleted);
        assert_eq!(observer.workers(), stale);
        assert!(fingerprints.is_empty());
    }

    #[test]
    fn authoritative_capture_merge_reports_group_deleted_without_merging_stale_capture() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let (mut state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        let captured = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        state.orchestration_groups.clear();
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            Some("authorized-before-capture"),
        )]);

        let outcome = merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(captured, "captured-after-deletion"),
        )
        .unwrap();

        assert_eq!(outcome, ObserverAuthorityOutcome::GroupDeleted);
        assert_eq!(
            observer.workers()[0].capture.as_deref(),
            Some("authorized-before-capture")
        );
    }

    #[test]
    fn navigation_preserves_refresh_failure_notice_until_refresh_succeeds() {
        let mut observer = ObserverState::new(vec![
            worker("first", ObserverLifecycle::Running, Some("one")),
            worker("second", ObserverLifecycle::Running, Some("two")),
        ]);
        apply_observer_refresh_result(
            &mut observer,
            Err(anyhow::anyhow!("raw backend failure /private/state.json")),
        );

        let outcome = observer.apply(ObserverAction::NextWorker);
        clear_transient_observer_notice(&mut observer, &outcome);
        assert_eq!(observer.notice(), Some(OBSERVER_REFRESH_FAILURE_NOTICE));

        assert_eq!(
            apply_observer_refresh_result(&mut observer, Ok(ObserverAuthorityOutcome::Authorized),),
            ObserverAuthorityOutcome::Authorized
        );
        assert!(observer.notice().is_none());
    }

    fn fingerprint(membership: &str, identity: &str) -> CaptureFingerprint {
        CaptureFingerprint {
            membership_id: membership.parse().unwrap(),
            session_id: "tether-0197f198000070008000000000000002".parse().unwrap(),
            ownership_proof: "0197f198000070008000000000000099".parse().unwrap(),
            tmux_session_id: identity.parse().unwrap(),
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: true,
                prompt_agent: false,
            },
        }
    }

    fn capture_state(membership: &str) -> (State, OrchestrationGroupId, SessionId) {
        let orchestrator = "tether-0197f198000070008000000000000001".parse().unwrap();
        let worker_id = "tether-0197f198000070008000000000000002".parse().unwrap();
        let group_id: OrchestrationGroupId = "group".parse().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        (
            State {
                version: State::CURRENT_VERSION,
                sessions: vec![SessionRecord {
                    herdr_agent: None,
                    id: worker_id,
                    host: "local".to_owned(),
                    target: "local".to_owned(),
                    directory: "/tmp".to_owned(),
                    preset: None,
                    command: Some("exec true".to_owned()),
                    tmux_session_id: Some("$7".parse().unwrap()),
                    ownership_proof: Some("0197f198000070008000000000000099".parse().unwrap()),
                    status: SessionStatus::Running,
                    created_at: now,
                    last_used_at: now,
                    closed_at: None,
                    exit_status: None,
                }],
                orchestration_groups: vec![OrchestrationGroup {
                    id: group_id.clone(),
                    title: "Group".parse().unwrap(),
                    orchestrator_session_id: orchestrator,
                    workers: vec![OrchestrationMember {
                        session_id: worker_id,
                        membership_id: membership.parse().unwrap(),
                        title: None,
                        capabilities: OrchestrationCapabilities {
                            observe_output: true,
                            open_interactive: true,
                            prompt_agent: false,
                        },
                    }],
                }],
            },
            group_id,
            worker_id,
        )
    }

    #[test]
    fn capture_backend_failure_becomes_unavailable_without_exposing_error_text() {
        let (mut state, _, _) = capture_state("0197f198000070008000000000000011");
        state.sessions[0].target = String::new();

        assert_eq!(
            capture_record(&state.sessions[0]),
            ObserverCapture::Unavailable
        );
    }

    fn capture_result(fingerprint: CaptureFingerprint, capture: &str) -> CaptureResult {
        CaptureResult {
            visible: HashSet::from([fingerprint.session_id.to_string()]),
            workers: vec![CapturedWorker {
                fingerprint,
                capture: ObserverCapture::Ready(capture.to_owned()),
                latency_ms: 23,
            }],
        }
    }

    #[test]
    fn metadata_refresh_preserves_captures_only_within_the_same_exact_epoch() {
        let mut observer = ObserverState::new(vec![
            worker("same", ObserverLifecycle::Running, Some("previous")),
            worker("changed", ObserverLifecycle::Running, Some("stale")),
        ]);
        let previous = HashMap::from([
            (
                "same".to_owned(),
                fingerprint("0197f198000070008000000000000011", "$7"),
            ),
            (
                "changed".to_owned(),
                fingerprint("0197f198000070008000000000000012", "$7"),
            ),
        ]);
        let current = HashMap::from([
            (
                "same".to_owned(),
                fingerprint("0197f198000070008000000000000011", "$7"),
            ),
            (
                "changed".to_owned(),
                fingerprint("0197f198000070008000000000000012", "$8"),
            ),
        ]);

        update_observer_metadata(
            &mut observer,
            &previous,
            &current,
            vec![
                worker("same", ObserverLifecycle::Running, None),
                worker("changed", ObserverLifecycle::Running, None),
                worker("new", ObserverLifecycle::Starting, None),
            ],
        );

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("previous"));
        assert_eq!(observer.workers()[1].capture, None);
        assert_eq!(observer.workers()[2].capture, None);
    }

    #[test]
    fn capture_merge_accepts_the_complete_current_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let (state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        let fingerprint = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            Some("old"),
        )]);

        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(fingerprint, "new"),
        )
        .unwrap();

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("new"));
    }

    #[test]
    fn remote_capture_loss_retains_last_output_as_stale_without_fake_latency() {
        let temp = tempfile::tempdir().unwrap();
        let (mut state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        state.sessions[0].target = "builder@example.test".to_owned();
        let fingerprint = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            None,
        )]);

        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(fingerprint.clone(), "last remote output"),
        )
        .unwrap();
        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            CaptureResult {
                visible: HashSet::from([worker_id.to_string()]),
                workers: vec![CapturedWorker {
                    fingerprint,
                    capture: ObserverCapture::Unavailable,
                    latency_ms: 10_000,
                }],
            },
        )
        .unwrap();

        let worker = &observer.workers()[0];
        assert_eq!(worker.capture.as_deref(), Some("last remote output"));
        assert_eq!(worker.agent_state, ObserverAgentState::Stale);
        assert_eq!(worker.latency_ms, Some(23));
        assert!(worker.last_observed.is_some());
    }

    #[test]
    fn capture_merge_rejects_a_changed_tmux_identity() {
        let temp = tempfile::tempdir().unwrap();
        let (mut state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        let captured = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        state.sessions[0].tmux_session_id = Some("$8".parse().unwrap());
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            Some("current"),
        )]);

        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(captured, "stale-$7"),
        )
        .unwrap();

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("current"));
    }

    #[test]
    fn capture_merge_rejects_capability_revocation() {
        let temp = tempfile::tempdir().unwrap();
        let (mut state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        let captured = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        state.orchestration_groups[0].workers[0]
            .capabilities
            .observe_output = false;
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            Some("current"),
        )]);

        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(captured, "revoked"),
        )
        .unwrap();

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("current"));
    }

    #[test]
    fn capture_merge_rejects_remove_and_readd_with_identical_visible_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let (mut state, group_id, worker_id) = capture_state("0197f198000070008000000000000011");
        let captured = capture_fingerprint(
            &state.orchestration_groups[0].workers[0],
            &state.sessions[0],
        )
        .unwrap();
        state.orchestration_groups[0].workers[0].membership_id =
            "0197f198000070008000000000000012".parse().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(&state).unwrap();
        let mut observer = ObserverState::new(vec![worker(
            &worker_id.to_string(),
            ObserverLifecycle::Running,
            Some("current"),
        )]);

        merge_captured_workers(
            &store,
            &group_id,
            &mut observer,
            capture_result(captured, "pre-removal"),
        )
        .unwrap();

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("current"));
    }

    #[test]
    fn remove_and_readd_allocates_a_new_persisted_membership_epoch() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let group_id: OrchestrationGroupId = "group".parse().unwrap();
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let worker_id: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: false,
            prompt_agent: false,
        };
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![
                    exact_running_session(orchestrator, "$1"),
                    exact_running_session(worker_id, "$2"),
                ],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        service
            .create_group(group_id.clone(), "Group".parse().unwrap(), orchestrator)
            .unwrap();
        let first = service
            .add_worker(&group_id, worker_id, None, capabilities)
            .unwrap();
        service.remove_worker(&group_id, worker_id).unwrap();
        let second = service
            .add_worker(&group_id, worker_id, None, capabilities)
            .unwrap();

        assert_ne!(first.membership_id, second.membership_id);
        assert_eq!(
            store.load().unwrap().orchestration_groups[0].workers[0].membership_id,
            second.membership_id
        );
    }

    #[test]
    fn ui_group_edits_are_atomic_and_preserve_retained_membership_epochs() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let group_id: OrchestrationGroupId = "observer-build".parse().unwrap();
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let first_worker: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let second_worker: SessionId = "tether-0197f198000070008000000000000003".parse().unwrap();
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![
                    exact_running_session(orchestrator, "$1"),
                    exact_running_session(first_worker, "$2"),
                    exact_running_session(second_worker, "$3"),
                ],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        };
        let first_spec = OrchestrationWorkerSpec {
            session_id: first_worker,
            title: Some("build worker".parse().unwrap()),
            capabilities,
        };

        let created = service
            .create_group_with_workers(
                group_id.clone(),
                "Observer build".parse().unwrap(),
                orchestrator,
                vec![first_spec.clone()],
            )
            .unwrap();
        let retained_epoch = created.workers[0].membership_id;
        let second_spec = OrchestrationWorkerSpec {
            session_id: second_worker,
            title: Some("test worker".parse().unwrap()),
            capabilities,
        };
        let edited = service
            .replace_workers(&created, vec![first_spec.clone(), second_spec.clone()])
            .unwrap();

        assert_eq!(edited.workers.len(), 2);
        assert_eq!(edited.workers[0].membership_id, retained_epoch);
        assert_eq!(edited.workers[1].session_id, second_worker);
        assert_ne!(edited.workers[1].membership_id, retained_epoch);

        let duplicate_error = service
            .replace_workers(&edited, vec![second_spec.clone(), second_spec])
            .unwrap_err()
            .to_string();
        assert!(
            duplicate_error.contains("duplicate worker"),
            "{duplicate_error}"
        );
        assert_eq!(
            store.load().unwrap().orchestration_groups[0].workers,
            edited.workers,
            "a rejected edit must not partially rewrite membership"
        );
    }

    #[test]
    fn ui_create_revalidates_exact_running_sessions_under_the_state_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let worker: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let spec = OrchestrationWorkerSpec {
            session_id: worker,
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: true,
                prompt_agent: false,
            },
        };
        let mut state = State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                exact_running_session(orchestrator, "$1"),
                exact_running_session(worker, "$2"),
            ],
            orchestration_groups: Vec::new(),
        };
        state.sessions[1].status = SessionStatus::Ended;
        state.sessions[1].closed_at = Some(state.sessions[1].last_used_at);
        store.save(&state).unwrap();

        let error = service
            .create_group_with_workers(
                "observer-stale-worker".parse().unwrap(),
                "Observer stale worker".parse().unwrap(),
                orchestrator,
                vec![spec.clone()],
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("running exact-owned"), "{error}");
        assert!(store.load().unwrap().orchestration_groups.is_empty());

        state.sessions[1] = exact_running_session(worker, "$2");
        state.sessions[0].ownership_proof = None;
        store.save(&state).unwrap();
        let error = service
            .create_group_with_workers(
                "observer-stale-orchestrator".parse().unwrap(),
                "Observer stale orchestrator".parse().unwrap(),
                orchestrator,
                vec![spec],
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("running exact-owned"), "{error}");
        assert!(store.load().unwrap().orchestration_groups.is_empty());
    }

    #[test]
    fn ui_edit_rejects_stale_snapshots_and_ineligible_new_workers() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let first: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let second: SessionId = "tether-0197f198000070008000000000000003".parse().unwrap();
        let stale_new: SessionId = "tether-0197f198000070008000000000000004".parse().unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        };
        let spec = |session_id| OrchestrationWorkerSpec {
            session_id,
            title: None,
            capabilities,
        };
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![
                    exact_running_session(orchestrator, "$1"),
                    exact_running_session(first, "$2"),
                    exact_running_session(second, "$3"),
                    exact_running_session(stale_new, "$4"),
                ],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        let group_id: OrchestrationGroupId = "observer-conflict".parse().unwrap();
        let displayed = service
            .create_group_with_workers(
                group_id.clone(),
                "Observer conflict".parse().unwrap(),
                orchestrator,
                vec![spec(first)],
            )
            .unwrap();
        service
            .add_worker(&group_id, second, None, capabilities)
            .unwrap();

        let error = service
            .replace_workers(&displayed, vec![spec(first)])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("changed while the manager was open"),
            "{error}"
        );
        let current = service.group(&group_id).unwrap();
        assert_eq!(current.workers.len(), 2);

        store
            .update(|state| {
                let record = state
                    .sessions
                    .iter_mut()
                    .find(|record| record.id == stale_new)
                    .unwrap();
                record.status = SessionStatus::Ended;
                record.closed_at = Some(record.last_used_at);
                Ok(())
            })
            .unwrap();
        let error = service
            .replace_workers(&current, vec![spec(first), spec(second), spec(stale_new)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("running exact-owned"), "{error}");
        assert_eq!(service.group(&group_id).unwrap(), current);

        store
            .update(|state| {
                state
                    .sessions
                    .iter_mut()
                    .filter(|record| record.id == first || record.id == second)
                    .for_each(|record| {
                        record.status = SessionStatus::Ended;
                        record.closed_at = Some(record.last_used_at);
                    });
                Ok(())
            })
            .unwrap();
        let retained = service
            .replace_workers(&current, vec![spec(first), spec(second)])
            .unwrap();
        assert_eq!(retained.workers, current.workers);
    }

    #[test]
    fn ui_delete_rejects_modified_and_recreated_groups() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let first: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let second: SessionId = "tether-0197f198000070008000000000000003".parse().unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        };
        let spec = |session_id| OrchestrationWorkerSpec {
            session_id,
            title: None,
            capabilities,
        };
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![
                    exact_running_session(orchestrator, "$1"),
                    exact_running_session(first, "$2"),
                    exact_running_session(second, "$3"),
                ],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        let group_id: OrchestrationGroupId = "observer-delete-conflict".parse().unwrap();
        let displayed = service
            .create_group_with_workers(
                group_id.clone(),
                "Observer delete conflict".parse().unwrap(),
                orchestrator,
                vec![spec(first)],
            )
            .unwrap();
        service
            .add_worker(&group_id, second, None, capabilities)
            .unwrap();

        let error = service
            .delete_group_if_unchanged(&displayed)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("changed while the manager was open"),
            "{error}"
        );
        let recreated_from = service.group(&group_id).unwrap();
        service.delete_group(&group_id).unwrap();
        service
            .create_group_with_workers(
                group_id.clone(),
                recreated_from.title.clone(),
                orchestrator,
                vec![spec(first), spec(second)],
            )
            .unwrap();

        let error = service
            .delete_group_if_unchanged(&recreated_from)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("changed while the manager was open"),
            "{error}"
        );
        assert_eq!(service.list_groups().unwrap().len(), 1);
    }
    #[test]
    fn companion_placement_always_preserves_the_source_pane() {
        assert_eq!(
            companion_placement(Placement::ReplaceCurrentPane),
            Placement::SplitRight
        );
        for placement in [
            Placement::SplitRight,
            Placement::SplitDown,
            Placement::NewTab,
        ] {
            assert_eq!(companion_placement(placement), placement);
        }
    }

    #[test]
    fn open_worker_revalidates_live_membership_capability_and_exact_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let orchestrator: SessionId = "tether-0197f198000070008000000000000001".parse().unwrap();
        let worker_id: SessionId = "tether-0197f198000070008000000000000002".parse().unwrap();
        let group_id: OrchestrationGroupId = "group".parse().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let record = SessionRecord {
            herdr_agent: None,
            id: worker_id,
            host: "local".to_owned(),
            target: "local".to_owned(),
            directory: "/tmp".to_owned(),
            preset: None,
            command: Some("exec true".to_owned()),
            tmux_session_id: Some("$7".parse().unwrap()),
            ownership_proof: Some("0197f198000070008000000000000099".parse().unwrap()),
            status: SessionStatus::Running,
            created_at: now,
            last_used_at: now,
            closed_at: None,
            exit_status: None,
        };
        let member = OrchestrationMember {
            session_id: worker_id,
            membership_id: OrchestrationMembershipId::new(),
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
                prompt_agent: false,
            },
        };
        let group = OrchestrationGroup {
            id: group_id.clone(),
            title: "Group".parse().unwrap(),
            orchestrator_session_id: orchestrator,
            workers: vec![member],
        };
        let mut state = State {
            version: State::CURRENT_VERSION,
            sessions: vec![record],
            orchestration_groups: vec![group],
        };
        store.save(&state).unwrap();
        let paths = AppPaths {
            config_file: temp.path().join("config.toml"),
            state_file: temp.path().join("state.json"),
            ssh_config_file: temp.path().join("ssh-config"),
        };
        let herdr_context = HerdrContext {
            binary: temp.path().join("unused-herdr"),
            pane_id: "pane".to_owned(),
            workspace_id: "workspace".to_owned(),
        };

        let error = open_worker(
            &paths,
            &store,
            &group_id,
            &worker_id.to_string(),
            &herdr_context,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not allow interactive open"), "{error}");

        state.orchestration_groups[0].workers[0]
            .capabilities
            .open_interactive = true;
        state.sessions[0].status = SessionStatus::Ended;
        state.sessions[0].closed_at = Some(now);
        store.save(&state).unwrap();
        let error = open_worker(
            &paths,
            &store,
            &group_id,
            &worker_id.to_string(),
            &herdr_context,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("not a running exact-owned session"),
            "{error}"
        );

        state.sessions[0].status = SessionStatus::Running;
        state.sessions[0].closed_at = None;
        state.sessions[0].ownership_proof = None;
        store.save(&state).unwrap();
        let error = open_worker(
            &paths,
            &store,
            &group_id,
            &worker_id.to_string(),
            &herdr_context,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("not a running exact-owned session"),
            "{error}"
        );

        state.orchestration_groups[0].workers.clear();
        store.save(&state).unwrap();
        let error = open_worker(
            &paths,
            &store,
            &group_id,
            &worker_id.to_string(),
            &herdr_context,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no longer a member"), "{error}");
    }

    /// A group with one member per interesting record shape.
    fn group_action_state(now: chrono::DateTime<Utc>) -> (State, OrchestrationGroupId) {
        let group_id: OrchestrationGroupId = "group".parse().unwrap();
        let base = SessionRecord {
            herdr_agent: None,
            id: "tether-0197f198000070008000000000000002".parse().unwrap(),
            host: "local".to_owned(),
            target: "local".to_owned(),
            directory: "/tmp".to_owned(),
            preset: None,
            command: Some("exec true".to_owned()),
            tmux_session_id: Some("$7".parse().unwrap()),
            ownership_proof: Some("0197f198000070008000000000000099".parse().unwrap()),
            status: SessionStatus::Running,
            created_at: now,
            last_used_at: now,
            closed_at: None,
            exit_status: None,
        };
        let running = base.clone();
        let legacy = SessionRecord {
            id: "tether-0197f198000070008000000000000003".parse().unwrap(),
            // A record from before ownership proofs. Tether cannot prove the
            // workload is its own, so nothing destructive may reach it.
            ownership_proof: None,
            ..base.clone()
        };
        let ended = SessionRecord {
            id: "tether-0197f198000070008000000000000004".parse().unwrap(),
            status: SessionStatus::Ended,
            closed_at: Some(now),
            exit_status: Some(0),
            ..base.clone()
        };
        // Failed inside the fast-failure window, so its restart is paced.
        let paced = SessionRecord {
            id: "tether-0197f198000070008000000000000005".parse().unwrap(),
            status: SessionStatus::Ended,
            last_used_at: Utc::now(),
            closed_at: Some(Utc::now()),
            exit_status: Some(1),
            ..base.clone()
        };
        let absent: SessionId = "tether-0197f198000070008000000000000006".parse().unwrap();
        let members = [running.id, legacy.id, ended.id, paced.id, absent]
            .into_iter()
            .map(|session_id| OrchestrationMember {
                session_id,
                membership_id: OrchestrationMembershipId::new(),
                title: None,
                capabilities: OrchestrationCapabilities {
                    observe_output: true,
                    open_interactive: true,
                    prompt_agent: false,
                },
            })
            .collect();
        (
            State {
                version: State::CURRENT_VERSION,
                sessions: vec![running, legacy, ended, paced],
                orchestration_groups: vec![OrchestrationGroup {
                    id: group_id.clone(),
                    title: "Group".parse().unwrap(),
                    orchestrator_session_id: "tether-0197f198000070008000000000000001"
                        .parse()
                        .unwrap(),
                    workers: members,
                }],
            },
            group_id,
        )
    }

    #[test]
    fn a_group_plan_never_offers_to_act_on_a_workload_tether_cannot_prove_it_owns() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (state, group_id) = group_action_state(now);
        store.save(&state).unwrap();
        let service = OrchestrationService::new(store);

        let stop = service
            .plan_group_action(&group_id, GroupAction::Stop)
            .unwrap();
        let acting: Vec<_> = stop.acting().map(|entry| entry.session_id).collect();
        assert_eq!(
            acting,
            vec![state.sessions[0].id],
            "only the running exact-owned member may be stopped"
        );
        let skipped: Vec<_> = stop.skipped().collect();
        assert_eq!(
            skipped,
            vec![
                (state.sessions[1].id, GroupSkip::NoOwnershipProof),
                (state.sessions[2].id, GroupSkip::NotRunning),
                (state.sessions[3].id, GroupSkip::NotRunning),
                (
                    "tether-0197f198000070008000000000000006".parse().unwrap(),
                    GroupSkip::MissingRecord
                ),
            ]
        );

        let restart = service
            .plan_group_action(&group_id, GroupAction::Restart)
            .unwrap();
        let acting: Vec<_> = restart.acting().map(|entry| entry.session_id).collect();
        assert_eq!(
            acting,
            vec![state.sessions[2].id],
            "a paced or running member is not restarted by a group"
        );
        let reasons: Vec<_> = restart.skipped().map(|(_, reason)| reason).collect();
        assert!(
            matches!(
                reasons.as_slice(),
                [
                    GroupSkip::NotEnded,
                    GroupSkip::NoOwnershipProof,
                    // The pace is measured from the recorded end, so the exact
                    // remainder depends on the clock; that it is withheld at all
                    // is the property.
                    GroupSkip::RestartPaced { seconds },
                    GroupSkip::MissingRecord,
                ] if (1..=30).contains(seconds)
            ),
            "a legacy record is refused before its status is even considered: {reasons:?}"
        );
    }

    #[test]
    fn applying_a_group_plan_skips_a_membership_that_changed_since_the_plan() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (mut state, group_id) = group_action_state(now);
        store.save(&state).unwrap();
        let service = OrchestrationService::new(store.clone());
        let plan = service
            .plan_group_action(&group_id, GroupAction::Stop)
            .unwrap();

        // A remove and re-add between the confirmation and the act is a new
        // authorization, so the confirmed one no longer applies.
        state.orchestration_groups[0].workers[0].membership_id = OrchestrationMembershipId::new();
        store.save(&state).unwrap();

        let lifecycle = LifecycleService::new(store, ProcessBinaries::new("ssh", "tmux"));
        let report = service.apply_group_action(&plan, &lifecycle).unwrap();
        assert_eq!(report.acted(), 0);
        assert_eq!(report.failed(), 0);
        assert_eq!(
            report
                .outcomes
                .iter()
                .find(|outcome| outcome.session_id == state.sessions[0].id)
                .map(|outcome| outcome.result.clone()),
            Some(GroupMemberResult::Skipped(GroupSkip::MembershipChanged))
        );
    }

    #[test]
    fn a_group_action_reports_each_member_on_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (state, group_id) = group_action_state(now);
        store.save(&state).unwrap();
        let service = OrchestrationService::new(store.clone());
        let plan = service
            .plan_group_action(&group_id, GroupAction::Stop)
            .unwrap();
        // No `tmux` here, so the one eligible member's stop fails in transport.
        // The refusals around it are still reported, and one failure does not
        // abandon the rest of the group.
        let lifecycle = LifecycleService::new(
            store,
            ProcessBinaries::new(temp.path().join("ssh"), temp.path().join("tmux")),
        );
        let report = service.apply_group_action(&plan, &lifecycle).unwrap();
        assert_eq!(report.outcomes.len(), 5);
        assert_eq!(report.acted(), 0);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.skipped(), 4);
        let failure = report
            .outcomes
            .iter()
            .find(|outcome| matches!(outcome.result, GroupMemberResult::Failed(_)))
            .expect("the eligible member reports its own failure");
        assert_eq!(failure.session_id, state.sessions[0].id);
        let GroupMemberResult::Failed(text) = &failure.result else {
            unreachable!();
        };
        assert!(!text.contains('\n'), "one bounded line: {text:?}");
    }

    #[test]
    fn adapter_role_admissions_require_live_exact_owned_sessions_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let candidate: SessionId = "tether-0197f198000070008000000000000041".parse().unwrap();

        for (case, session) in [
            ("absent", None),
            (
                "ended",
                Some({
                    let mut record = exact_running_session(candidate, "$41");
                    record.status = SessionStatus::Ended;
                    record.closed_at = Some(record.last_used_at);
                    record
                }),
            ),
            (
                "proofless",
                Some({
                    let mut record = exact_running_session(candidate, "$41");
                    record.ownership_proof = None;
                    record
                }),
            ),
            (
                "identity-less",
                Some({
                    let mut record = exact_running_session(candidate, "$41");
                    record.tmux_session_id = None;
                    record
                }),
            ),
        ] {
            let before = State {
                version: State::CURRENT_VERSION,
                sessions: session.into_iter().collect(),
                orchestration_groups: Vec::new(),
            };
            store.save(&before).unwrap();

            let error = service
                .create_group(
                    format!("invalid-{case}").parse().unwrap(),
                    "Invalid".parse().unwrap(),
                    candidate,
                )
                .unwrap_err()
                .to_string();

            assert!(
                error.contains("running exact-owned workload"),
                "{case}: {error}"
            );
            assert_eq!(store.load().unwrap(), before, "{case}");
        }
    }

    #[test]
    fn adapter_add_worker_rejects_current_orchestrator_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let store = StateStore::new(path.clone());
        let service = OrchestrationService::new(store.clone());
        let previous_orchestrator: SessionId =
            "tether-0197f198000070008000000000000050".parse().unwrap();
        let current_orchestrator: SessionId =
            "tether-0197f198000070008000000000000051".parse().unwrap();
        let retained: SessionId = "tether-0197f198000070008000000000000052".parse().unwrap();
        let group_id: OrchestrationGroupId = "adapter-current-orchestrator".parse().unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: false,
            prompt_agent: false,
        };
        let retained_member = OrchestrationMember {
            session_id: retained,
            membership_id: "0197f198000070008000000000000052".parse().unwrap(),
            title: Some("retained".parse().unwrap()),
            capabilities,
        };
        let before = State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                exact_running_session(previous_orchestrator, "$50"),
                exact_running_session(current_orchestrator, "$51"),
            ],
            orchestration_groups: vec![OrchestrationGroup {
                id: group_id.clone(),
                title: "Current topology".parse().unwrap(),
                orchestrator_session_id: current_orchestrator,
                workers: vec![retained_member],
            }],
        };
        store.save(&before).unwrap();
        let persisted_before = std::fs::read(&path).unwrap();

        let error = service
            .add_worker(
                &group_id,
                current_orchestrator,
                Some("invalid worker".parse().unwrap()),
                capabilities,
            )
            .unwrap_err()
            .to_string();

        assert_eq!(error, "orchestrator must not also be a worker");
        assert_eq!(store.load().unwrap(), before);
        assert_eq!(std::fs::read(path).unwrap(), persisted_before);
    }

    #[test]
    fn adapter_add_worker_revalidates_before_mutation_and_retains_unavailable_members() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let orchestrator: SessionId = "tether-0197f198000070008000000000000051".parse().unwrap();
        let retained: SessionId = "tether-0197f198000070008000000000000052".parse().unwrap();
        let candidate: SessionId = "tether-0197f198000070008000000000000053".parse().unwrap();
        let group_id: OrchestrationGroupId = "adapter-add".parse().unwrap();
        let capabilities = OrchestrationCapabilities {
            observe_output: true,
            open_interactive: false,
            prompt_agent: false,
        };
        let retained_member = OrchestrationMember {
            session_id: retained,
            membership_id: "0197f198000070008000000000000052".parse().unwrap(),
            title: Some("retained unavailable".parse().unwrap()),
            capabilities,
        };
        let base = State {
            version: State::CURRENT_VERSION,
            sessions: vec![exact_running_session(candidate, "$53")],
            orchestration_groups: vec![OrchestrationGroup {
                id: group_id.clone(),
                title: "Adapter add".parse().unwrap(),
                orchestrator_session_id: orchestrator,
                workers: vec![retained_member.clone()],
            }],
        };
        store.save(&base).unwrap();

        let added = service
            .add_worker(
                &group_id,
                candidate,
                Some("new".parse().unwrap()),
                capabilities,
            )
            .unwrap();
        let persisted = store.load().unwrap();
        assert_eq!(
            persisted.orchestration_groups[0].workers[0],
            retained_member
        );
        assert_eq!(persisted.orchestration_groups[0].workers[1], added);

        for (case, session) in [
            ("absent", None),
            (
                "ended",
                Some({
                    let mut record = exact_running_session(candidate, "$53");
                    record.status = SessionStatus::Ended;
                    record.closed_at = Some(record.last_used_at);
                    record
                }),
            ),
            (
                "proofless",
                Some({
                    let mut record = exact_running_session(candidate, "$53");
                    record.ownership_proof = None;
                    record
                }),
            ),
            (
                "identity-less",
                Some({
                    let mut record = exact_running_session(candidate, "$53");
                    record.tmux_session_id = None;
                    record
                }),
            ),
        ] {
            let mut before = base.clone();
            before.sessions = session.into_iter().collect();
            store.save(&before).unwrap();
            let error = service
                .add_worker(&group_id, candidate, None, capabilities)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("running exact-owned workload"),
                "{case}: {error}"
            );
            assert_eq!(store.load().unwrap(), before, "{case}");
        }
    }

    #[test]
    fn reassign_orchestrator_preserves_group_and_unaffected_worker_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let old_orchestrator: SessionId =
            "tether-0197f198000070008000000000000061".parse().unwrap();
        let promoted: SessionId = "tether-0197f198000070008000000000000062".parse().unwrap();
        let unaffected: SessionId = "tether-0197f198000070008000000000000063".parse().unwrap();
        let non_worker: SessionId = "tether-0197f198000070008000000000000064".parse().unwrap();
        let group_id: OrchestrationGroupId = "reassign".parse().unwrap();
        let first = OrchestrationMember {
            session_id: promoted,
            membership_id: "0197f198000070008000000000000062".parse().unwrap(),
            title: Some("promote me".parse().unwrap()),
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
                prompt_agent: false,
            },
        };
        let second = OrchestrationMember {
            session_id: unaffected,
            membership_id: "0197f198000070008000000000000063".parse().unwrap(),
            title: Some("leave me".parse().unwrap()),
            capabilities: OrchestrationCapabilities {
                observe_output: false,
                open_interactive: true,
                prompt_agent: false,
            },
        };
        let expected = OrchestrationGroup {
            id: group_id,
            title: "Stable identity".parse().unwrap(),
            orchestrator_session_id: old_orchestrator,
            workers: vec![first, second.clone()],
        };
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![
                    exact_running_session(promoted, "$62"),
                    exact_running_session(unaffected, "$63"),
                    exact_running_session(non_worker, "$64"),
                ],
                orchestration_groups: vec![expected.clone()],
            })
            .unwrap();

        let promoted_group = service.reassign_orchestrator(&expected, promoted).unwrap();
        assert_eq!(promoted_group.id, expected.id);
        assert_eq!(promoted_group.title, expected.title);
        assert_eq!(promoted_group.orchestrator_session_id, promoted);
        assert_eq!(promoted_group.workers, vec![second.clone()]);
        assert!(
            promoted_group
                .workers
                .iter()
                .all(|worker| worker.session_id != promoted)
        );
        assert!(
            promoted_group
                .workers
                .iter()
                .all(|worker| worker.session_id != old_orchestrator)
        );

        let reassigned = service
            .reassign_orchestrator(&promoted_group, non_worker)
            .unwrap();
        assert_eq!(reassigned.workers, vec![second]);
        assert_eq!(reassigned.orchestrator_session_id, non_worker);
    }

    #[test]
    fn reassign_orchestrator_rejects_conflicts_and_ineligible_replacements_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let service = OrchestrationService::new(store.clone());
        let old_orchestrator: SessionId =
            "tether-0197f198000070008000000000000071".parse().unwrap();
        let candidate: SessionId = "tether-0197f198000070008000000000000072".parse().unwrap();
        let current = OrchestrationGroup {
            id: "reassign-conflict".parse().unwrap(),
            title: "Current".parse().unwrap(),
            orchestrator_session_id: old_orchestrator,
            workers: Vec::new(),
        };
        let state = State {
            version: State::CURRENT_VERSION,
            sessions: vec![exact_running_session(candidate, "$72")],
            orchestration_groups: vec![current.clone()],
        };
        store.save(&state).unwrap();
        let mut stale = current.clone();
        stale.title = "Stale".parse().unwrap();

        let error = service
            .reassign_orchestrator(&stale, candidate)
            .unwrap_err()
            .to_string();
        assert_eq!(error, MANAGER_STALE_GROUP_ERROR);
        assert_eq!(store.load().unwrap(), state);

        for (case, session) in [
            ("absent", None),
            (
                "ended",
                Some({
                    let mut record = exact_running_session(candidate, "$72");
                    record.status = SessionStatus::Ended;
                    record.closed_at = Some(record.last_used_at);
                    record
                }),
            ),
            (
                "proofless",
                Some({
                    let mut record = exact_running_session(candidate, "$72");
                    record.ownership_proof = None;
                    record
                }),
            ),
            (
                "identity-less",
                Some({
                    let mut record = exact_running_session(candidate, "$72");
                    record.tmux_session_id = None;
                    record
                }),
            ),
        ] {
            let before = State {
                version: State::CURRENT_VERSION,
                sessions: session.into_iter().collect(),
                orchestration_groups: vec![current.clone()],
            };
            store.save(&before).unwrap();
            let error = service
                .reassign_orchestrator(&current, candidate)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("running exact-owned workload"),
                "{case}: {error}"
            );
            assert_eq!(store.load().unwrap(), before, "{case}");
        }
    }
}
