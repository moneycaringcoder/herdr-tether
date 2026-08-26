//! Pure projection and rendering for the read-only orchestration observer.
//!
//! This module deliberately knows nothing about sessions, tmux, or any orchestration
//! harness. Runtime integrations translate their state into [`ObserverWorker`] values
//! and translate [`ObserverOutcome`] values back into application actions.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
};

use crate::model::{SessionId, TmuxSessionId};

use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

pub const WORKERS_PER_PAGE: usize = 4;
pub const MAX_WORKERS: usize = 64;
pub const MAX_CAPTURE_LINES: usize = 200;
pub const MAX_CAPTURE_BYTES: usize = 16 * 1024;
pub const MAX_CAPTURE_CELLS: usize = 16 * 1024;
pub const MAX_PROMPT_TARGETS: usize = 8;
const MIN_TILE_WIDTH: u16 = 12;
const MIN_TILE_HEIGHT: u16 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObserverCapabilities {
    pub observe_output: bool,
    pub open_interactive: bool,
    pub prompt_agent: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObserverLifecycle {
    Starting,
    Running,
    Stopping,
    Ended,
    /// The command ended with a failing status, which `tmux` reported.
    ///
    /// Kept apart from [`Self::Ended`] because a clean finish and a failure are
    /// the two outcomes a person most needs to tell apart, and they were
    /// previously the same word. An end whose status could not be read stays
    /// `Ended`: an unknown outcome is not a failure.
    Failed {
        exit_status: i32,
    },
    Missing,
    Removed,
    #[default]
    Unknown,
}

impl ObserverLifecycle {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Ended => "ENDED",
            Self::Failed { .. } => "FAILED",
            Self::Missing => "MISSING",
            Self::Removed => "REMOVED",
            Self::Unknown => "UNKNOWN",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObserverAgentState {
    Detached,
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
    Unreachable,
    Stale,
}

impl ObserverAgentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Detached => "DETACHED",
            Self::Idle => "IDLE",
            Self::Working => "WORKING",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Unknown => "UNKNOWN",
            Self::Unreachable => "UNREACHABLE",
            Self::Stale => "STALE",
        }
    }

    pub const fn permits_prompt(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverCapture {
    Loading,
    Ready(String),
    /// Output Herdr reported as incomplete because older rows were dropped.
    ///
    /// Herdr 0.8.0 reports `truncated: true` on pane and agent reads whenever
    /// it omitted scrollback. Keeping this distinct from [`Self::Ready`] stops
    /// a clipped capture from reading as the worker's complete output.
    Truncated(String),
    /// A bounded sample of recent output, taken to show what a workload is
    /// doing at a glance.
    ///
    /// Distinct from [`Self::Ready`] because it is deliberately not everything:
    /// a tile showing a sample must say so, or a reader will take the last few
    /// lines for the whole story.
    Preview {
        text: String,
        /// Whether Herdr reported it dropped older rows from the sample.
        truncated: bool,
    },
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureStatus {
    Loading,
    Ready,
    Truncated,
    Unavailable,
    Stale,
}

struct PreviousWorkerState {
    capture: Option<String>,
    sampled: bool,
    last_observed: Option<String>,
    latency_ms: Option<u64>,
    live_agent: bool,
    agent_state: ObserverAgentState,
    lifecycle: ObserverLifecycle,
    incarnation: Option<TmuxSessionId>,
}

/// A worker that just entered a state a person may want to know about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerAttention {
    pub worker_id: String,
    /// How the workload is named outside Tether's own surfaces.
    ///
    /// Deliberately not the tile's display title. That title is generated from
    /// the workload's host, repository name, and preset, and a notification
    /// leaves the surface that produced it, so it carries a reference to the
    /// work instead of a description of it. The tile beside it still shows the
    /// friendly title.
    pub reference: String,
    pub reason: AttentionReason,
}

/// A workload reference that identifies the work without describing it.
///
/// The tail of the session id is enough for a person to match a notification to
/// the row or tile in front of them, and it says nothing about where the work
/// runs or what it does.
fn workload_reference(worker_id: &str) -> String {
    match worker_id.char_indices().rev().nth(7) {
        Some((index, _)) if index > 0 => format!("…{}", &worker_id[index..]),
        _ => worker_id.to_owned(),
    }
}

/// Why a worker wants attention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionReason {
    /// A live agent entered a state that waits on a person.
    Agent(ObserverAgentState),
    /// The workload's command ended with a failing status.
    Failed { exit_status: i32 },
}

/// Why a worker's information is no longer current.
///
/// Each one has its own remedy, which is the whole point of telling them apart:
/// a lost connection comes back on its own, and a binding that is no longer
/// exact does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaleReason {
    /// The pane that answered belongs to an earlier membership of this worker.
    ///
    /// Usually the group was edited while Mission Control was open, so the claim
    /// simply moved on and a resnapshot is genuinely enough.
    EarlierMembership,
    /// More than one Herdr pane claims this worker's membership.
    ///
    /// Nothing Tether can do settles which one is real, so the remedy is in
    /// Herdr: one of the two panes has to go.
    AmbiguousClaim,
    /// Something other than the agent Tether bound is in the worker's pane.
    ///
    /// A resnapshot reports the same newcomer every time, so the only remedy is
    /// to open the workload again.
    ReplacedOccupant,
    /// Mission Control lost the connection that was reporting this worker.
    Connection,
    /// The observed output could not be re-read, so what is shown is retained.
    Retained,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverWorker {
    pub id: String,
    pub title: Option<String>,
    pub capabilities: ObserverCapabilities,
    pub lifecycle: ObserverLifecycle,
    pub agent_state: ObserverAgentState,
    pub live_agent: bool,
    pub owned: bool,
    pub last_observed: Option<String>,
    pub latency_ms: Option<u64>,
    /// The exact `tmux` incarnation this worker's state describes.
    ///
    /// A restart produces a new one, which is what tells a repeated failure
    /// apart from the same failure seen again. The exit status cannot: a command
    /// that keeps failing keeps failing the same way.
    pub incarnation: Option<TmuxSessionId>,
    /// Untrusted capture content. Rendering sanitizes and bounds it before display.
    pub capture: Option<String>,
    /// Whether the capture is a bounded sample rather than everything Herdr has.
    ///
    /// A tile that showed a sample as if it were the whole output would invite a
    /// reader to conclude nothing else happened, so this follows the text
    /// everywhere the text goes.
    pub sampled: bool,
    /// Why this worker is `STALE`, when it is.
    ///
    /// The reasons have different remedies, and the capture status cannot tell
    /// them apart: it is a separate axis, set by a separate path. A tile that
    /// guessed from it would blame the binding for a lost connection.
    pub stale_reason: Option<StaleReason>,
}

impl ObserverWorker {
    pub fn can_open(&self) -> bool {
        self.owned
            && self.lifecycle == ObserverLifecycle::Running
            && !matches!(
                self.agent_state,
                ObserverAgentState::Unreachable | ObserverAgentState::Stale
            )
            && self.capabilities.open_interactive
    }
    pub fn can_focus(&self) -> bool {
        self.owned
            && self.lifecycle == ObserverLifecycle::Running
            && self.live_agent
            && !matches!(
                self.agent_state,
                ObserverAgentState::Unreachable | ObserverAgentState::Stale
            )
            && self.capabilities.open_interactive
    }

    pub fn can_observe_agent(&self) -> bool {
        self.owned
            && self.lifecycle == ObserverLifecycle::Running
            && self.live_agent
            && !matches!(
                self.agent_state,
                ObserverAgentState::Unreachable | ObserverAgentState::Stale
            )
            && self.capabilities.observe_output
    }

    pub fn can_prompt(&self) -> bool {
        self.owned
            && self.lifecycle == ObserverLifecycle::Running
            && self.live_agent
            && self.agent_state.permits_prompt()
            && self.capabilities.prompt_agent
    }

    pub const fn uses_live_agent(&self) -> bool {
        self.live_agent
    }

    pub const fn status_label(&self) -> &'static str {
        if !self.live_agent || !matches!(self.lifecycle, ObserverLifecycle::Running) {
            self.lifecycle.label()
        } else {
            self.agent_state.label()
        }
    }

    fn human_display_title(&self) -> Option<String> {
        self.title
            .as_deref()
            .filter(|title| !title.is_empty())
            .map(|title| sanitize_label(title, 160))
    }

    fn display_title(&self) -> String {
        self.human_display_title().unwrap_or_else(|| {
            self.id
                .parse::<SessionId>()
                .map(|id| id.reference_token(SessionId::SHORT_REFERENCE_WIDTH))
                .unwrap_or_else(|_| sanitize_label(&self.id, 160))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverAction {
    PreviousWorker,
    NextWorker,
    PreviousPage,
    NextPage,
    Refresh,
    OpenSelected,
    TogglePromptTarget,
    ComposePrompt,
    FocusSelected,
    WaitSelected,
    ReadSelected,
    ExplainSelected,
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverOutcome {
    None,
    Refresh,
    OpenSelected { worker_id: String },
    OpenUnavailable { worker_id: String },
    ComposePrompt { worker_ids: Vec<String> },
    FocusSelected { worker_id: String },
    WaitSelected { worker_id: String },
    ReadSelected { worker_id: String },
    ExplainSelected { worker_id: String },
    Quit,
}

/// Backend-independent keys accepted by the observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverKey {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    ControlC,
    Enter,
    Escape,
    Char(char),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverInputKind {
    Press,
    Repeat,
}

/// Maps a navigation key to a read-only observer action.
///
/// There is intentionally no action carrying arbitrary text or terminal bytes.
pub fn action_for_key(key: ObserverKey) -> Option<ObserverAction> {
    match key {
        ObserverKey::Up | ObserverKey::Left | ObserverKey::Char('k' | 'h') => {
            Some(ObserverAction::PreviousWorker)
        }
        ObserverKey::Down | ObserverKey::Right | ObserverKey::Char('j' | 'l') => {
            Some(ObserverAction::NextWorker)
        }
        ObserverKey::PageUp | ObserverKey::BackTab | ObserverKey::Char('[') => {
            Some(ObserverAction::PreviousPage)
        }
        ObserverKey::PageDown | ObserverKey::Tab | ObserverKey::Char(']') => {
            Some(ObserverAction::NextPage)
        }
        ObserverKey::Enter => Some(ObserverAction::OpenSelected),
        ObserverKey::Char(' ') => Some(ObserverAction::TogglePromptTarget),
        ObserverKey::Char('p' | 'P') => Some(ObserverAction::ComposePrompt),
        ObserverKey::Char('f' | 'F') => Some(ObserverAction::FocusSelected),
        ObserverKey::Char('w' | 'W') => Some(ObserverAction::WaitSelected),
        ObserverKey::Char('v' | 'V') => Some(ObserverAction::ReadSelected),
        ObserverKey::Char('e' | 'E') => Some(ObserverAction::ExplainSelected),
        ObserverKey::Char('r' | 'R') => Some(ObserverAction::Refresh),
        ObserverKey::Escape | ObserverKey::ControlC | ObserverKey::Char('q' | 'Q') => {
            Some(ObserverAction::Quit)
        }
        ObserverKey::Char(_) => None,
    }
}

/// Applies event-kind and in-flight-operation gating to Observer keys.
///
/// Navigation is repeat-safe and remains available while an operation is busy.
/// Open and refresh are single-shot and idle-only. Quit remains available while
/// busy, but release and repeat events never duplicate it.
pub fn action_for_input(
    key: ObserverKey,
    kind: ObserverInputKind,
    busy: bool,
) -> Option<ObserverAction> {
    gate_action_for_input(action_for_key(key)?, kind, busy)
}

fn gate_action_for_input(
    action: ObserverAction,
    kind: ObserverInputKind,
    busy: bool,
) -> Option<ObserverAction> {
    match action {
        ObserverAction::PreviousWorker
        | ObserverAction::NextWorker
        | ObserverAction::PreviousPage
        | ObserverAction::NextPage => Some(action),
        ObserverAction::Refresh
        | ObserverAction::OpenSelected
        | ObserverAction::TogglePromptTarget
        | ObserverAction::ComposePrompt
        | ObserverAction::FocusSelected
        | ObserverAction::WaitSelected
        | ObserverAction::ReadSelected
        | ObserverAction::ExplainSelected
            if kind == ObserverInputKind::Press && !busy =>
        {
            Some(action)
        }
        ObserverAction::Quit if kind == ObserverInputKind::Press => Some(action),
        ObserverAction::Refresh
        | ObserverAction::OpenSelected
        | ObserverAction::TogglePromptTarget
        | ObserverAction::ComposePrompt
        | ObserverAction::FocusSelected
        | ObserverAction::WaitSelected
        | ObserverAction::ReadSelected
        | ObserverAction::ExplainSelected
        | ObserverAction::Quit => None,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObserverState {
    workers: Vec<ObserverWorker>,
    capture_statuses: HashMap<String, CaptureStatus>,
    selected_id: Option<String>,
    prompt_targets: HashSet<String>,
    notice: Option<String>,
}

impl ObserverState {
    pub fn new(workers: Vec<ObserverWorker>) -> Self {
        let mut state = Self::default();
        state.update_workers(workers);
        state
    }

    pub fn workers(&self) -> &[ObserverWorker] {
        &self.workers
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Projects a key through the actions that are possible in the current view.
    ///
    /// With no workers there is nothing to select, page through, or open. Refresh
    /// and Back remain available so an empty observer is never a dead end.
    pub fn action_for_key(&self, key: ObserverKey) -> Option<ObserverAction> {
        let action = action_for_key(key)?;
        if self.workers.is_empty()
            && !matches!(action, ObserverAction::Refresh | ObserverAction::Quit)
        {
            return None;
        }
        let available = match action {
            ObserverAction::TogglePromptTarget | ObserverAction::ComposePrompt => self
                .workers
                .iter()
                .any(|worker| worker.capabilities.prompt_agent),
            ObserverAction::FocusSelected => self.workers.iter().any(ObserverWorker::can_focus),
            ObserverAction::WaitSelected
            | ObserverAction::ReadSelected
            | ObserverAction::ExplainSelected => {
                self.workers.iter().any(ObserverWorker::can_observe_agent)
            }
            _ => true,
        };
        available.then_some(action)
    }

    /// Projects a key through both view availability and input event gating.
    pub fn action_for_input(
        &self,
        key: ObserverKey,
        kind: ObserverInputKind,
        busy: bool,
    ) -> Option<ObserverAction> {
        gate_action_for_input(self.action_for_key(key)?, kind, busy)
    }

    /// Replaces membership while preserving selection and capture state by worker identity.
    ///
    /// A supplied capture, including an empty capture, completes loading. An absent
    /// capture preserves the prior lifecycle for an existing worker and starts a new
    /// worker in loading. Duplicate IDs after their first occurrence and workers beyond
    /// [`MAX_WORKERS`] are ignored. If the selected identity disappeared, the prior
    /// numeric position is retained where possible.
    /// Replaces the worker set and reports newly attention-worthy workers.
    ///
    /// A transition is only reported when a live agent *changes into* `BLOCKED`
    /// or `DONE`, or when a workload *changes into* a failing end. Re-reporting
    /// a state the worker was already in would turn every refresh into a
    /// notification.
    pub fn update_workers(&mut self, workers: Vec<ObserverWorker>) -> Vec<WorkerAttention> {
        let previous_index = self.selected_index().unwrap_or(0);
        let previous_workers: HashMap<String, PreviousWorkerState> = self
            .workers
            .drain(..)
            .map(|worker| {
                (
                    worker.id,
                    PreviousWorkerState {
                        capture: worker.capture,
                        sampled: worker.sampled,
                        last_observed: worker.last_observed,
                        live_agent: worker.live_agent,
                        latency_ms: worker.latency_ms,
                        agent_state: worker.agent_state,
                        lifecycle: worker.lifecycle,
                        incarnation: worker.incarnation,
                    },
                )
            })
            .collect();
        let previous_statuses = std::mem::take(&mut self.capture_statuses);
        let mut seen = HashSet::with_capacity(workers.len().min(MAX_WORKERS));
        self.workers = workers
            .into_iter()
            .filter(|worker| seen.insert(worker.id.clone()))
            .take(MAX_WORKERS)
            .map(|mut worker| {
                let status = if let Some(capture) = worker.capture.take() {
                    worker.capture = Some(sanitize_capture(&capture));
                    CaptureStatus::Ready
                } else {
                    let status = previous_statuses
                        .get(&worker.id)
                        .copied()
                        .unwrap_or(CaptureStatus::Loading);
                    let previous = previous_workers.get(&worker.id);
                    if matches!(status, CaptureStatus::Ready | CaptureStatus::Stale)
                        && let Some(previous) = previous
                    {
                        // A metadata refresh carries no capture, so the retained
                        // one is kept - along with whether it was a sample, or
                        // the next refresh would present it as everything.
                        worker.capture.clone_from(&previous.capture);
                        worker.sampled = previous.sampled;
                    }
                    if worker.last_observed.is_none()
                        && let Some(previous) = previous
                    {
                        worker.last_observed.clone_from(&previous.last_observed);
                    }
                    if worker.latency_ms.is_none()
                        && let Some(previous) = previous
                    {
                        worker.latency_ms = previous.latency_ms;
                    }
                    let lost_live_agent = previous.is_some_and(|previous| previous.live_agent)
                        && worker.agent_state == ObserverAgentState::Unreachable;
                    if lost_live_agent {
                        worker.live_agent = true;
                    }
                    // A worker that arrives stale carries the reason it was made
                    // stale for, when there is one: the projection names which
                    // binding failure it was. Nothing is invented here, because a
                    // guessed reason would send a reader to the wrong remedy.
                    if lost_live_agent
                        || (worker.capture.is_some()
                            && matches!(
                                worker.agent_state,
                                ObserverAgentState::Unreachable | ObserverAgentState::Stale
                            ))
                    {
                        if worker.agent_state == ObserverAgentState::Unreachable {
                            worker.stale_reason = Some(if lost_live_agent {
                                StaleReason::Connection
                            } else {
                                StaleReason::Retained
                            });
                        }
                        worker.agent_state = ObserverAgentState::Stale;
                        CaptureStatus::Stale
                    } else if matches!(
                        worker.agent_state,
                        ObserverAgentState::Idle
                            | ObserverAgentState::Working
                            | ObserverAgentState::Blocked
                            | ObserverAgentState::Done
                    ) && status == CaptureStatus::Stale
                    {
                        if worker.capture.is_some() {
                            CaptureStatus::Ready
                        } else {
                            CaptureStatus::Loading
                        }
                    } else {
                        status
                    }
                };
                self.capture_statuses.insert(worker.id.clone(), status);
                worker
            })
            .collect();
        let agents = self
            .workers
            .iter()
            .filter(|worker| worker.live_agent)
            .filter(|worker| {
                matches!(
                    worker.agent_state,
                    ObserverAgentState::Blocked | ObserverAgentState::Done
                )
            })
            .filter(|worker| {
                previous_workers
                    .get(&worker.id)
                    .is_none_or(|previous| previous.agent_state != worker.agent_state)
            })
            .map(|worker| WorkerAttention {
                worker_id: worker.id.clone(),
                reference: workload_reference(&worker.id),
                reason: AttentionReason::Agent(worker.agent_state),
            });
        // A failing end is reported once per incarnation. A worker seen for the
        // first time already failed before this Observer opened, which is
        // history rather than news; a worker that stays failed is not news
        // either. A restart that fails again is a different incarnation, and a
        // command that fails the same way twice is the ordinary case, so the
        // exit status cannot be what tells them apart.
        let failures = self
            .workers
            .iter()
            .filter_map(|worker| match worker.lifecycle {
                ObserverLifecycle::Failed { exit_status } => Some((worker, exit_status)),
                _ => None,
            })
            .filter(|(worker, _)| {
                previous_workers.get(&worker.id).is_some_and(|previous| {
                    !matches!(previous.lifecycle, ObserverLifecycle::Failed { .. })
                        || previous.incarnation != worker.incarnation
                })
            })
            .map(|(worker, exit_status)| WorkerAttention {
                worker_id: worker.id.clone(),
                reference: workload_reference(&worker.id),
                reason: AttentionReason::Failed { exit_status },
            });
        let attention: Vec<WorkerAttention> = agents.chain(failures).collect();
        self.prompt_targets.retain(|id| {
            self.workers
                .iter()
                .any(|worker| worker.id == *id && worker.can_prompt())
        });

        if self.workers.is_empty() {
            self.selected_id = None;
            self.prompt_targets.clear();
            return attention;
        }
        if self
            .selected_id
            .as_deref()
            .is_none_or(|selected| !self.workers.iter().any(|worker| worker.id == selected))
        {
            let index = previous_index.min(self.workers.len() - 1);
            self.selected_id = Some(self.workers[index].id.clone());
        }
        attention
    }

    pub fn set_connection_observation(
        &mut self,
        worker_id: &str,
        latency_ms: u64,
        observed: Option<String>,
    ) -> bool {
        let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| worker.id == worker_id)
        else {
            return false;
        };
        worker.latency_ms = Some(latency_ms);
        if observed.is_some() {
            worker.last_observed = observed;
        }
        true
    }

    /// Merges one capture result without changing worker identity or membership.
    pub fn merge_capture(&mut self, worker_id: &str, capture: ObserverCapture) -> bool {
        let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| worker.id == worker_id)
        else {
            return false;
        };
        let status = match capture {
            ObserverCapture::Loading => {
                worker.capture = None;
                worker.sampled = false;
                CaptureStatus::Loading
            }
            // An explicit read is everything Herdr offered, so it clears any
            // sample marker the tile was carrying.
            ObserverCapture::Ready(capture) => {
                worker.capture = Some(sanitize_capture(&capture));
                worker.sampled = false;
                CaptureStatus::Ready
            }
            ObserverCapture::Truncated(capture) => {
                worker.capture = Some(sanitize_capture(&capture));
                worker.sampled = false;
                CaptureStatus::Truncated
            }
            ObserverCapture::Preview { text, truncated } => {
                worker.capture = Some(sanitize_capture(&text));
                worker.sampled = true;
                if truncated {
                    CaptureStatus::Truncated
                } else {
                    CaptureStatus::Ready
                }
            }
            ObserverCapture::Unavailable
                if worker.capture.is_some() && worker.last_observed.is_some() =>
            {
                worker.agent_state = ObserverAgentState::Stale;
                worker.stale_reason = Some(StaleReason::Retained);
                CaptureStatus::Stale
            }
            ObserverCapture::Unavailable => {
                worker.capture = None;
                if worker.capabilities.prompt_agent {
                    worker.agent_state = ObserverAgentState::Unreachable;
                }
                CaptureStatus::Unavailable
            }
        };
        self.capture_statuses.insert(worker_id.to_owned(), status);
        true
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }
    pub fn prompt_target_ids(&self) -> Vec<String> {
        self.workers
            .iter()
            .filter(|worker| self.prompt_targets.contains(&worker.id))
            .map(|worker| worker.id.clone())
            .collect()
    }

    pub fn is_prompt_target(&self, worker_id: &str) -> bool {
        self.prompt_targets.contains(worker_id)
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected_id.as_deref()?;
        self.workers.iter().position(|worker| worker.id == selected)
    }

    pub fn selected_worker(&self) -> Option<&ObserverWorker> {
        self.selected_index().map(|index| &self.workers[index])
    }

    pub fn page(&self) -> usize {
        self.selected_index().unwrap_or(0) / WORKERS_PER_PAGE
    }

    pub fn page_count(&self) -> usize {
        self.workers.len().max(1).div_ceil(WORKERS_PER_PAGE)
    }

    pub fn visible_workers(&self) -> &[ObserverWorker] {
        let start = self.page() * WORKERS_PER_PAGE;
        let end = (start + WORKERS_PER_PAGE).min(self.workers.len());
        &self.workers[start..end]
    }

    fn worker_display_title(&self, worker: &ObserverWorker) -> String {
        let Some(title) = worker.human_display_title() else {
            return worker.display_title();
        };
        let matching_ids: Vec<SessionId> = self
            .workers
            .iter()
            .filter(|candidate| candidate.human_display_title().as_deref() == Some(&title))
            .filter_map(|candidate| candidate.id.parse().ok())
            .collect();
        if matching_ids.len() < 2 {
            return title;
        }
        let Ok(worker_id) = worker.id.parse::<SessionId>() else {
            return title;
        };
        let width = (SessionId::SHORT_REFERENCE_WIDTH..=SessionId::MAX_REFERENCE_WIDTH)
            .find(|width| {
                let mut references = HashSet::with_capacity(matching_ids.len());
                matching_ids
                    .iter()
                    .all(|id| references.insert(id.reference_token(*width)))
            })
            .unwrap_or(SessionId::MAX_REFERENCE_WIDTH);
        format!("{title} · {}", worker_id.reference_token(width))
    }

    pub fn apply(&mut self, action: ObserverAction) -> ObserverOutcome {
        match action {
            ObserverAction::PreviousWorker => self.select_offset(-1),
            ObserverAction::NextWorker => self.select_offset(1),
            ObserverAction::PreviousPage => self.select_page_offset(-1),
            ObserverAction::NextPage => self.select_page_offset(1),
            ObserverAction::Refresh => return ObserverOutcome::Refresh,
            ObserverAction::OpenSelected => {
                if let Some(worker) = self.selected_worker() {
                    return if worker.can_open() {
                        ObserverOutcome::OpenSelected {
                            worker_id: worker.id.clone(),
                        }
                    } else {
                        ObserverOutcome::OpenUnavailable {
                            worker_id: worker.id.clone(),
                        }
                    };
                }
            }
            ObserverAction::TogglePromptTarget => {
                let Some((id, can_prompt)) = self
                    .selected_worker()
                    .map(|worker| (worker.id.clone(), worker.can_prompt()))
                else {
                    return ObserverOutcome::None;
                };
                if self.prompt_targets.remove(&id) {
                    self.notice = None;
                } else if !can_prompt {
                    self.notice = Some(
                        "Prompt requires exact-owned, authorized, IDLE or DONE agent".to_owned(),
                    );
                } else if self.prompt_targets.len() >= MAX_PROMPT_TARGETS {
                    self.notice = Some(format!(
                        "At most {MAX_PROMPT_TARGETS} prompt destinations may be selected"
                    ));
                } else {
                    self.prompt_targets.insert(id);
                    self.notice = None;
                }
            }
            ObserverAction::ComposePrompt => {
                if self.prompt_targets.is_empty()
                    && let Some((id, true)) = self
                        .selected_worker()
                        .map(|worker| (worker.id.clone(), worker.can_prompt()))
                {
                    self.prompt_targets.insert(id);
                }
                let worker_ids = self.prompt_target_ids();
                if worker_ids.is_empty() {
                    self.notice =
                        Some("Select at least one prompt-authorized IDLE or DONE agent".to_owned());
                } else {
                    return ObserverOutcome::ComposePrompt { worker_ids };
                }
            }
            ObserverAction::FocusSelected => {
                if let Some((worker_id, true)) = self
                    .selected_worker()
                    .map(|worker| (worker.id.clone(), worker.can_focus()))
                {
                    return ObserverOutcome::FocusSelected { worker_id };
                }
                self.notice = Some(
                    "Focus requires an exact live agent with interactive-open permission"
                        .to_owned(),
                );
            }
            ObserverAction::WaitSelected => {
                if let Some((worker_id, true)) = self
                    .selected_worker()
                    .map(|worker| (worker.id.clone(), worker.can_observe_agent()))
                {
                    return ObserverOutcome::WaitSelected { worker_id };
                }
                self.notice = Some(
                    "Wait requires an exact live agent with observation permission".to_owned(),
                );
            }
            ObserverAction::ReadSelected => {
                if let Some((worker_id, true)) = self
                    .selected_worker()
                    .map(|worker| (worker.id.clone(), worker.can_observe_agent()))
                {
                    return ObserverOutcome::ReadSelected { worker_id };
                }
                self.notice = Some(
                    "Read requires an exact live agent with observation permission".to_owned(),
                );
            }
            ObserverAction::ExplainSelected => {
                if let Some((worker_id, true)) = self
                    .selected_worker()
                    .map(|worker| (worker.id.clone(), worker.can_observe_agent()))
                {
                    return ObserverOutcome::ExplainSelected { worker_id };
                }
                self.notice = Some(
                    "Explain requires an exact live agent with observation permission".to_owned(),
                );
            }
            ObserverAction::Quit => return ObserverOutcome::Quit,
        }
        ObserverOutcome::None
    }

    fn select_offset(&mut self, offset: isize) {
        if self.workers.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = current
            .saturating_add_signed(offset)
            .min(self.workers.len() - 1);
        self.selected_id = Some(self.workers[next].id.clone());
    }

    fn select_page_offset(&mut self, offset: isize) {
        if self.workers.is_empty() {
            return;
        }
        let page = self.page();
        let next_page = page
            .saturating_add_signed(offset)
            .min(self.page_count().saturating_sub(1));
        let index = (next_page * WORKERS_PER_PAGE).min(self.workers.len() - 1);
        self.selected_id = Some(self.workers[index].id.clone());
    }
}

/// Returns deterministic row-major worker tiles for a page.
pub fn worker_rects(area: Rect, count: usize) -> Vec<Rect> {
    let count = count.min(WORKERS_PER_PAGE);
    match count {
        0 => Vec::new(),
        1 => vec![area],
        2 => {
            let left = area.width / 2;
            vec![
                Rect::new(area.x, area.y, left, area.height),
                Rect::new(
                    area.x.saturating_add(left),
                    area.y,
                    area.width - left,
                    area.height,
                ),
            ]
        }
        3 | 4 => {
            let left = area.width / 2;
            let top = area.height / 2;
            let widths = [left, area.width - left];
            let heights = [top, area.height - top];
            (0..count)
                .map(|index| {
                    let column = index % 2;
                    let row = index / 2;
                    Rect::new(
                        area.x
                            .saturating_add(if column == 0 { 0 } else { widths[0] }),
                        area.y.saturating_add(if row == 0 { 0 } else { heights[0] }),
                        widths[column],
                        heights[row],
                    )
                })
                .collect()
        }
        _ => unreachable!(),
    }
}
pub(crate) fn observer_theme_style(selected: bool) -> Style {
    let style = Style::default().fg(Color::Reset).bg(Color::Reset);
    if selected {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn can_render_worker_grid(canvas: Rect, count: usize) -> bool {
    match count.min(WORKERS_PER_PAGE) {
        0 => true,
        1 => canvas.width >= MIN_TILE_WIDTH && canvas.height >= MIN_TILE_HEIGHT,
        2 => canvas.width / 2 >= MIN_TILE_WIDTH && canvas.height >= MIN_TILE_HEIGHT,
        3 | 4 => canvas.width / 2 >= MIN_TILE_WIDTH && canvas.height / 2 >= MIN_TILE_HEIGHT,
        _ => unreachable!(),
    }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, observer: &ObserverState) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(observer_theme_style(false)), area);

    let worker_count = observer.workers.len();
    let mission_control = observer
        .workers
        .iter()
        .any(|worker| worker.live_agent || worker.capabilities.prompt_agent);
    let prompt_available = observer
        .workers
        .iter()
        .any(|worker| worker.capabilities.prompt_agent);
    let surface_name = if mission_control {
        "Mission Control"
    } else {
        "Observer"
    };
    let controls_height = if worker_count == 0 || area.width >= 64 {
        1
    } else if area.width >= 30 {
        2
    } else {
        3
    };
    let notice_height = u16::from(observer.notice().is_some());
    let bottom_height = controls_height + notice_height;
    let visible_count = observer.visible_workers().len();
    let canvas_height = area.height.saturating_sub(1 + bottom_height);
    let canvas = Rect::new(area.x, area.y.saturating_add(1), area.width, canvas_height);
    if area.height < 2 + bottom_height
        || (visible_count > 0 && !can_render_worker_grid(canvas, visible_count))
    {
        frame.render_widget(
            Paragraph::new(format!("{surface_name}\nResize pane"))
                .style(observer_theme_style(false)),
            area,
        );
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let noun = if worker_count == 1 {
        "worker"
    } else {
        "workers"
    };
    let header_text = if worker_count == 0 {
        format!("{surface_name}  0 workers")
    } else {
        format!(
            "{surface_name}  {worker_count} {noun}  page {}/{}",
            observer.page() + 1,
            observer.page_count()
        )
    };
    frame.render_widget(
        Paragraph::new(header_text).style(observer_theme_style(false)),
        header,
    );

    let visible = observer.visible_workers();
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("No workers registered\nPress r to refresh")
                .style(observer_theme_style(false)),
            canvas,
        );
    } else {
        for (worker, rect) in visible.iter().zip(worker_rects(canvas, visible.len())) {
            render_worker(
                frame,
                rect,
                &TileView {
                    worker,
                    capture_status: observer
                        .capture_statuses
                        .get(&worker.id)
                        .copied()
                        .unwrap_or(CaptureStatus::Loading),
                    display_title: &observer.worker_display_title(worker),
                    selected: observer.selected_id() == Some(worker.id.as_str()),
                    prompt_target: observer.is_prompt_target(&worker.id),
                    refresh_verb: if mission_control { "retry" } else { "refresh" },
                },
            );
        }
    }

    let mut footer_y = canvas.y.saturating_add(canvas.height);
    if let Some(notice) = observer.notice() {
        let notice_area = Rect::new(area.x, footer_y, area.width, 1);
        let notice = format!("! {}", sanitize_capture(notice).replace('\n', " "));
        frame.render_widget(
            Paragraph::new(notice).style(observer_theme_style(false)),
            notice_area,
        );
        footer_y = footer_y.saturating_add(1);
    }

    let hidden = worker_count.saturating_sub((observer.page() + 1) * WORKERS_PER_PAGE);
    let controls = if worker_count == 0 {
        "r refresh  q back".to_owned()
    } else if !mission_control && controls_height == 1 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("+{hidden} more  ")
        };
        format!("{overflow}↑↓ select  Tab/[ ] page  r refresh  Enter open  q back")
    } else if !mission_control && controls_height == 2 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!("↑↓ select  [] page  r refresh\nEnter open  q back{overflow}")
    } else if !mission_control {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!("↑↓ select  [] page\nr refresh  Enter open\nq back{overflow}")
    } else if !prompt_available && controls_height == 1 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("+{hidden} more  ")
        };
        widest_that_fits(
            area.width,
            &[
                format!(
                    "{overflow}↑↓ select  Enter open  f focus  v read  w wait  e explain  r retry  q back"
                ),
                format!(
                    "{overflow}↑↓ select  Enter open  f focus  v read  w wait  r retry  q back"
                ),
            ],
        )
    } else if !prompt_available && controls_height == 2 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!(
            "↑↓ select  Enter open  f focus\nv read  w wait  e explain  r retry  q back{overflow}"
        )
    } else if !prompt_available {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!(
            "↑↓ select  Enter open\nf focus  v read  w wait  e explain\nr retry  q back{overflow}"
        )
    } else if controls_height == 1 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("+{hidden} more  ")
        };
        widest_that_fits(
            area.width,
            &[
                format!(
                    "{overflow}↑↓ select  Space target  p prompt  Enter open  f focus  v read  w wait  e explain  r retry  q back"
                ),
                format!(
                    "{overflow}↑↓ select  Space target  p prompt  Enter open  f focus  e explain  r retry  q back"
                ),
                format!(
                    "{overflow}↑↓ select  Space target  p prompt  Enter open  f focus  r retry  q back"
                ),
            ],
        )
    } else if controls_height == 2 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!(
            "↑↓ select  Space target  p prompt  Enter open\nf focus  v read  w wait  e explain  r retry  q back{overflow}"
        )
    } else {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!(
            "↑↓ select  Space target\np prompt  Enter open  f focus\nv read  w wait  e explain  r retry  q back{overflow}"
        )
    };
    let controls_area = Rect::new(area.x, footer_y, area.width, controls_height);
    frame.render_widget(
        Paragraph::new(controls).style(observer_theme_style(false)),
        controls_area,
    );
}
/// When a worker was last seen live, for dating what a tile is still showing.
fn last_live(worker: &ObserverWorker) -> String {
    worker
        .last_observed
        .as_deref()
        .map(|value| sanitize_label(value, 80))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Everything a tile needs about the worker it draws.
struct TileView<'a> {
    worker: &'a ObserverWorker,
    capture_status: CaptureStatus,
    display_title: &'a str,
    selected: bool,
    prompt_target: bool,
    /// The word the footer of this surface uses for the `r` key, so a tile never
    /// names it differently from the controls beneath it.
    refresh_verb: &'a str,
}

fn render_worker(frame: &mut Frame<'_>, area: Rect, view: &TileView<'_>) {
    if area.is_empty() {
        return;
    }
    let TileView {
        worker,
        capture_status,
        display_title,
        selected,
        prompt_target,
        refresh_verb,
    } = *view;
    let marker = if selected {
        "▶ "
    } else if prompt_target {
        "✓ "
    } else {
        "  "
    };
    let eligibility = if worker.can_prompt() {
        " · PROMPT"
    } else if worker.can_open() {
        " · OPEN"
    } else {
        ""
    };
    let latency = worker
        .latency_ms
        .map(|latency| format!(" · {latency}ms"))
        .unwrap_or_default();
    // The body marker is the first thing a short or narrow tile drops, and an
    // unlabelled sample reads as the whole output. The title is the fallback: it
    // costs no body row and is clipped last.
    let sampled = if worker.sampled { " · PREVIEW" } else { "" };
    let title = format!(
        "{marker}{} · {}{latency}{sampled}{eligibility}",
        display_title,
        worker.status_label()
    );
    let style = observer_theme_style(selected);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title))
        .style(style);
    // Both of these states mean the tile is not telling the truth right now, for
    // different reasons and with different remedies, so the tile says which. Each
    // sentence is built from what this tile can see - the state, why it went
    // stale, and whether anything is retained - because a sentence that outran
    // the facts would contradict the rows underneath it.
    let inner_width = area.width.saturating_sub(2);
    let retained = worker.capture.is_some();
    let state_line = match worker.agent_state {
        ObserverAgentState::Unreachable if retained => Some(widest_that_fits(
            inner_width,
            &[
                format!(
                    "UNREACHABLE · nothing answered · the output below is not current · r {refresh_verb}"
                ),
                format!(
                    "UNREACHABLE · nothing answered · output is not current · r {refresh_verb}"
                ),
                format!("UNREACHABLE · output not current · r {refresh_verb}"),
                format!("UNREACHABLE · r {refresh_verb}"),
            ],
        )),
        ObserverAgentState::Unreachable => Some(widest_that_fits(
            inner_width,
            &[
                format!(
                    "UNREACHABLE · Herdr or the host did not answer · nothing retained · r {refresh_verb}"
                ),
                format!("UNREACHABLE · no answer, nothing retained · r {refresh_verb}"),
                format!("UNREACHABLE · nothing retained · r {refresh_verb}"),
                format!("UNREACHABLE · r {refresh_verb}"),
            ],
        )),
        // Each of these is a different problem with a different remedy, and the
        // source already tells them apart: a membership that moved on comes back
        // with a resnapshot, two panes claiming one worker is a decision only the
        // operator can make in Herdr, and a pane something else took has to be
        // opened again.
        ObserverAgentState::Stale
            if worker.stale_reason == Some(StaleReason::EarlierMembership) =>
        {
            Some(widest_that_fits(
                inner_width,
                &[
                    format!("STALE · pane belongs to an earlier membership · r {refresh_verb}"),
                    format!("STALE · earlier membership · r {refresh_verb}"),
                    format!("STALE · r {refresh_verb}"),
                ],
            ))
        }
        ObserverAgentState::Stale if worker.stale_reason == Some(StaleReason::AmbiguousClaim) => {
            Some(widest_that_fits(
                inner_width,
                &[
                    format!(
                        "STALE · two Herdr panes claim this worker · close one in Herdr, then r {refresh_verb}"
                    ),
                    "STALE · two panes claim this worker · close one in Herdr".to_owned(),
                    "STALE · two panes claim it · close one in Herdr".to_owned(),
                    // Every rung keeps the action, because the retry the other
                    // reasons fall back to is the one thing that cannot help
                    // here: it reports the same two claims every time. A tile is
                    // half the canvas as soon as there are two workers, so these
                    // short rungs are what a real page shows.
                    "STALE · two panes claim it · close one".to_owned(),
                    "STALE · 2 panes claim it · close one".to_owned(),
                    "STALE · 2 claims · close one".to_owned(),
                ],
            ))
        }
        ObserverAgentState::Stale if worker.stale_reason == Some(StaleReason::ReplacedOccupant) => {
            Some(widest_that_fits(
                inner_width,
                &[
                    "STALE · another agent took this pane · reopen the workload".to_owned(),
                    "STALE · another agent took this pane · reopen it".to_owned(),
                    // As with two claims, every rung keeps the action: a
                    // resnapshot reports the same newcomer every time, so the
                    // one thing the shorter lines must not fall back to is the
                    // retry.
                    "STALE · another agent took it · reopen it".to_owned(),
                    "STALE · pane taken · reopen it".to_owned(),
                    "STALE · reopen it".to_owned(),
                ],
            ))
        }
        ObserverAgentState::Stale if worker.stale_reason == Some(StaleReason::Connection) => {
            let observed = last_live(worker);
            Some(widest_that_fits(
                inner_width,
                &[
                    format!(
                        "STALE · connection lost · showing last known · last live {observed} · r {refresh_verb}"
                    ),
                    format!("STALE · connection lost · last live {observed} · r {refresh_verb}"),
                    format!("STALE · last live {observed} · r {refresh_verb}"),
                    format!("STALE · connection lost · r {refresh_verb}"),
                    format!("STALE · r {refresh_verb}"),
                ],
            ))
        }
        ObserverAgentState::Stale if retained => {
            let observed = last_live(worker);
            Some(widest_that_fits(
                inner_width,
                &[
                    format!(
                        "STALE · retained, not current · last live {observed} · r {refresh_verb}"
                    ),
                    // The time it was last live outranks the prose: it is the
                    // fact that tells a reader how old what they see is.
                    format!("STALE · not current · last live {observed} · r {refresh_verb}"),
                    format!("STALE · last live {observed} · r {refresh_verb}"),
                    format!("STALE · retained, not current · r {refresh_verb}"),
                    format!("STALE · r {refresh_verb}"),
                ],
            ))
        }
        // Stale with nothing retained: there is no remembered output to offer,
        // so the sentence must not imply there is.
        ObserverAgentState::Stale => Some(widest_that_fits(
            inner_width,
            &[
                format!("STALE · not current, and nothing retained · r {refresh_verb}"),
                format!("STALE · not current, nothing retained · r {refresh_verb}"),
                format!("STALE · not current · r {refresh_verb}"),
                format!("STALE · r {refresh_verb}"),
            ],
        )),
        _ => None,
    };
    // An explanation is added to what the body already said, never swapped for
    // it: why output is missing and why the tile is not current are different
    // facts, and a retry fixes only one of them.
    let prefixed = |line: Option<String>, body: &str| match line {
        Some(line) => format!("{line}\n{body}"),
        None => body.to_owned(),
    };
    let body = if !worker.capabilities.observe_output {
        prefixed(state_line, "Output not authorized")
    } else {
        match capture_status {
            // A tile that is not current must not advertise a read key the
            // surface refuses in that state.
            CaptureStatus::Loading if worker.uses_live_agent() && state_line.is_none() => {
                "Herdr agent attached · press v to read output".to_owned()
            }
            CaptureStatus::Loading => prefixed(state_line, "Loading output"),
            CaptureStatus::Unavailable => prefixed(state_line, "Output unavailable"),
            CaptureStatus::Stale => {
                let capture = worker
                    .capture
                    .as_deref()
                    .map(sanitize_capture)
                    .unwrap_or_default();
                // A retained sample is still a sample, so the marker travels with
                // it even when the tile is no longer current.
                let sampled = if worker.sampled { " · PREVIEW" } else { "" };
                let explanation = state_line.unwrap_or_else(|| "STALE".to_owned());
                format!("{explanation}{sampled}\n{capture}")
            }
            status @ (CaptureStatus::Ready | CaptureStatus::Truncated) => {
                // A sample says so, without naming a line count: what is shown is
                // whatever survived Herdr's answer, the display bounds, and the
                // rows this tile has, so a number here would overstate it.
                let sampled = worker.sampled.then(|| {
                    widest_that_fits(
                        inner_width,
                        &[
                            "PREVIEW · recent output · v for more".to_owned(),
                            "PREVIEW · v for more".to_owned(),
                            "PREVIEW".to_owned(),
                        ],
                    )
                });
                // Every line above the output costs a row of it, and none may cost
                // the last one: on a tile too short to hold both, the output wins.
                // Offered in order of what has no fallback elsewhere: truncation is
                // only ever said here, while the state and the sample are both in
                // the border title as well.
                let mut reserved = 2;
                let mut prefix_lines = Vec::new();
                for line in [
                    (status == CaptureStatus::Truncated)
                        .then(|| "TRUNCATED · older output dropped by Herdr".to_owned()),
                    sampled,
                    state_line,
                ]
                .into_iter()
                .flatten()
                {
                    if area.height <= reserved + 1 {
                        break;
                    }
                    reserved += 1;
                    prefix_lines.push(line);
                }
                let prefix = prefix_lines.join("\n");
                worker
                    .capture
                    .as_deref()
                    .map(sanitize_capture)
                    .filter(|capture| !capture.is_empty())
                    .map(|capture| {
                        let viewport = capture_viewport(
                            &capture,
                            area.width.saturating_sub(2),
                            area.height.saturating_sub(reserved),
                        );
                        if prefix.is_empty() {
                            viewport
                        } else {
                            format!("{prefix}\n{viewport}")
                        }
                    })
                    .unwrap_or_else(|| {
                        if prefix.is_empty() {
                            "No captured output".to_owned()
                        } else {
                            prefix.clone()
                        }
                    })
            }
        }
    };
    frame.render_widget(
        Paragraph::new(body)
            .style(observer_theme_style(false))
            .block(block),
        area,
    );
}

/// Picks the first single-line control string that fits the available width.
///
/// The one-line footer is used whenever the pane is at least 64 columns, which
/// spans everything from a narrow split to a full-width tab. Listing candidates
/// richest-first lets a wide surface advertise every control while an 80-column
/// terminal keeps the shorter layout its keyboard checks pin. The last candidate
/// is the floor and is returned even when it overflows.
fn widest_that_fits(width: u16, candidates: &[String]) -> String {
    let width = usize::from(width);
    candidates
        .iter()
        .find(|candidate| candidate.chars().count() <= width)
        .unwrap_or_else(|| candidates.last().expect("at least one control layout"))
        .clone()
}

#[derive(Debug)]
pub enum ObserverRenderError {}

impl fmt::Display for ObserverRenderError {
    fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl std::error::Error for ObserverRenderError {}

/// Renders an observer into a deterministic plain-text terminal buffer.
pub fn render_to_text(
    width: u16,
    height: u16,
    observer: &ObserverState,
) -> Result<String, ObserverRenderError> {
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).map_err(|error| match error {})?;
    terminal
        .draw(|frame| render(frame, frame.area(), observer))
        .map_err(|error| match error {})?;
    let buffer = terminal.backend().buffer();
    let mut output = String::with_capacity((usize::from(width) + 1) * usize::from(height));
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        if y + 1 < height {
            output.push('\n');
        }
    }
    Ok(output)
}

#[cfg(test)]
pub(crate) fn render_to_styles(
    width: u16,
    height: u16,
    observer: &ObserverState,
) -> Result<Vec<(Color, Color, Modifier)>, ObserverRenderError> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).map_err(|error| match error {})?;
    terminal
        .draw(|frame| render(frame, frame.area(), observer))
        .map_err(|error| match error {})?;
    Ok(terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| (cell.fg, cell.bg, cell.modifier))
        .collect())
}

/// Removes terminal escapes and unsafe formatting characters, normalizes line
/// endings/tabs, and retains the newest bounded output by UTF-8 bytes, logical
/// lines, and display cells.
pub fn sanitize_capture(input: &str) -> String {
    let mut output = String::with_capacity(input.len().min(MAX_CAPTURE_BYTES));
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            consume_escape(&mut chars);
            continue;
        }
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push('\n');
            }
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            value if is_unsafe_format(value) || value.is_control() => {}
            value => output.push(value),
        }
    }
    bounded_capture_tail(&output)
}

fn bounded_capture_tail(input: &str) -> String {
    let mut start = input.len();
    let mut lines = 1usize;
    let mut bytes = 0usize;
    let mut cells = 0usize;
    for (index, cluster) in input.grapheme_indices(true).rev() {
        let cluster_bytes = cluster.len();
        if cluster == "\n" {
            if lines >= MAX_CAPTURE_LINES || bytes + cluster_bytes > MAX_CAPTURE_BYTES {
                break;
            }
            lines += 1;
        } else {
            let width = display_width(cluster);
            if bytes + cluster_bytes > MAX_CAPTURE_BYTES || cells + width > MAX_CAPTURE_CELLS {
                break;
            }
            cells += width;
        }
        bytes += cluster_bytes;
        start = index;
    }
    input[start..].to_owned()
}

fn capture_viewport(input: &str, width: u16, height: u16) -> String {
    let width = usize::from(width);
    let height = usize::from(height);
    if width == 0 || height == 0 {
        return String::new();
    }
    let mut rows = VecDeque::with_capacity(height);
    for line in input.split('\n') {
        let mut row = String::new();
        let mut cells = 0usize;
        for cluster in line.graphemes(true) {
            let cluster_width = display_width(cluster);
            if cells > 0 && cells + cluster_width > width {
                push_viewport_row(&mut rows, std::mem::take(&mut row), height);
                cells = 0;
            }
            if cluster_width <= width {
                row.push_str(cluster);
                cells += cluster_width;
            }
        }
        push_viewport_row(&mut rows, row, height);
    }
    rows.into_iter().collect::<Vec<_>>().join("\n")
}

fn push_viewport_row(rows: &mut VecDeque<String>, row: String, height: usize) {
    if rows.len() == height {
        rows.pop_front();
    }
    rows.push_back(row);
}

fn consume_escape<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.next() {
        Some('[') => {
            for character in chars.by_ref() {
                if ('@'..='~').contains(&character) {
                    break;
                }
            }
        }
        Some(']' | 'P' | 'X' | '^' | '_') => {
            let mut escaped = false;
            for character in chars.by_ref() {
                if character == '\u{7}' {
                    break;
                }
                if escaped && character == '\\' {
                    break;
                }
                escaped = character == '\u{1b}';
            }
        }
        Some(_) | None => {}
    }
}

fn sanitize_label(input: &str, max_cells: usize) -> String {
    let capture = sanitize_capture(input);
    let mut output = String::new();
    let mut cells = 0;
    for cluster in capture.graphemes(true) {
        let width = if cluster == "\n" {
            1
        } else {
            display_width(cluster)
        };
        if cells + width > max_cells {
            break;
        }
        if cluster == "\n" {
            output.push(' ');
        } else {
            output.push_str(cluster);
        }
        cells += width;
    }
    output
}

fn is_unsafe_format(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'..='\u{200c}'
            | '\u{200e}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn display_width(cluster: &str) -> usize {
    Line::from(cluster).width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_renderer_exposes_only_terminal_default_colors() {
        let observer = ObserverState::new(Vec::new());
        assert!(render_to_styles(12, 3, &observer).unwrap().iter().all(
            |(foreground, background, _)| {
                *foreground == Color::Reset && *background == Color::Reset
            }
        ));
    }
}
