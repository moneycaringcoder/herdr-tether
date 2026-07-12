//! Pure state, input, and rendering for the native Observer manager.

use std::{
    collections::{HashMap, HashSet},
    fmt, io,
};

use anyhow::{Context, Result};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::{CrosstermBackend, TestBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use unicode_segmentation::UnicodeSegmentation;

use crate::{
    model::{OrchestrationGroupId, OrchestrationTitle, SessionId},
    orchestration::OrchestrationWorkerSpec,
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, compare_normal_sessions,
    },
};

const MAX_PANEL_WIDTH: u16 = 72;
const MAX_VISIBLE_ROWS: usize = 12;
const MIN_MANAGER_WIDTH: u16 = 40;
const MIN_MANAGER_HEIGHT: u16 = 8;
const MANAGER_RESIZE_MESSAGE: &str = "Resize terminal to at least 40x8";
const DEFAULT_CAPABILITIES: OrchestrationCapabilities = OrchestrationCapabilities {
    observe_output: true,
    open_interactive: true,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverManagerScreen {
    Groups,
    CreateOrchestrator,
    CreateWorkers,
    GroupActions,
    EditWorkers,
    ChangeOrchestrator,
    ReviewTopology,
    ConfirmDelete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverManagerEvent {
    Previous,
    Next,
    Confirm,
    Back,
    Toggle,
    Create,
    Edit,
    Delete,
    ConfirmDelete,
    DismissDelete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverManagerAction {
    Create {
        id: OrchestrationGroupId,
        title: OrchestrationTitle,
        orchestrator_session_id: SessionId,
        workers: Vec<OrchestrationWorkerSpec>,
    },
    ReplaceWorkers {
        expected_group: OrchestrationGroup,
        workers: Vec<OrchestrationWorkerSpec>,
    },
    ReassignOrchestrator {
        expected_group: OrchestrationGroup,
        orchestrator_session_id: SessionId,
    },
    Delete {
        expected_group: OrchestrationGroup,
    },
    Launch {
        group_id: OrchestrationGroupId,
    },
    BackToPicker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverManagerOutcome {
    Continue,
    Action(ObserverManagerAction),
}

#[derive(Clone, Debug)]
struct Candidate {
    session_id: SessionId,
    label: String,
    host: String,
    repository: String,
    title: OrchestrationTitle,
    existing: Option<OrchestrationMember>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewKind {
    Create,
    ReplaceWorkers,
    ReassignOrchestrator,
}

#[derive(Clone, Debug)]
pub struct ObserverManagerState {
    screen: ObserverManagerScreen,
    groups: Vec<OrchestrationGroup>,
    eligible: Vec<Candidate>,
    screen_candidates: Vec<Candidate>,
    session_labels: HashMap<SessionId, String>,
    running_sessions: HashSet<SessionId>,
    selected_index: usize,
    selected_group: Option<usize>,
    selected_orchestrator: Option<SessionId>,
    selected_workers: HashSet<SessionId>,
    review_kind: Option<ReviewKind>,
    review_return_index: usize,
    notice: Option<String>,
}

impl ObserverManagerState {
    pub fn from_state(state: &State, notice: Option<String>) -> Result<Self> {
        let labels = candidate_labels(&state.sessions);
        let mut eligible = state
            .sessions
            .iter()
            .filter(|record| is_eligible(record))
            .map(|record| {
                candidate(
                    record,
                    labels.get(&record.id).expect("eligible label exists"),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        eligible.sort_by(|left, right| {
            let left_record = state
                .sessions
                .iter()
                .find(|record| record.id == left.session_id)
                .expect("candidate session exists");
            let right_record = state
                .sessions
                .iter()
                .find(|record| record.id == right.session_id)
                .expect("candidate session exists");
            compare_normal_sessions(
                left_record.status,
                left_record.last_used_at,
                left_record.id,
                right_record.status,
                right_record.last_used_at,
                right_record.id,
            )
        });
        let session_labels = labels;
        let running_sessions = state
            .sessions
            .iter()
            .filter(|record| record.status == SessionStatus::Running)
            .map(|record| record.id)
            .collect();
        Ok(Self {
            screen: ObserverManagerScreen::Groups,
            groups: state.orchestration_groups.clone(),
            eligible,
            screen_candidates: Vec::new(),
            session_labels,
            running_sessions,
            selected_index: 0,
            selected_group: None,
            selected_orchestrator: None,
            selected_workers: HashSet::new(),
            review_kind: None,
            review_return_index: 0,
            notice,
        })
    }

    pub fn screen(&self) -> ObserverManagerScreen {
        self.screen
    }

    pub fn frame_title(&self) -> &'static str {
        match self.screen {
            ObserverManagerScreen::Groups => "Observers",
            ObserverManagerScreen::CreateOrchestrator => "Choose orchestrator",
            ObserverManagerScreen::CreateWorkers => "Choose workers",
            ObserverManagerScreen::GroupActions => "Observer actions",
            ObserverManagerScreen::EditWorkers => "Edit workers",
            ObserverManagerScreen::ChangeOrchestrator => "Change orchestrator",
            ObserverManagerScreen::ReviewTopology => "Review topology",
            ObserverManagerScreen::ConfirmDelete => "Confirm delete",
        }
    }

    pub fn item_labels(&self) -> Vec<String> {
        match self.screen {
            ObserverManagerScreen::Groups => {
                let mut labels = self
                    .groups
                    .iter()
                    .map(|group| self.group_summary(group))
                    .collect::<Vec<_>>();
                labels.push("+ Create Observer".to_owned());
                labels
            }
            ObserverManagerScreen::CreateOrchestrator
            | ObserverManagerScreen::ChangeOrchestrator => self
                .screen_candidates
                .iter()
                .map(|candidate| format!("ORCHESTRATOR {}", candidate.label))
                .collect(),
            ObserverManagerScreen::CreateWorkers | ObserverManagerScreen::EditWorkers => self
                .screen_candidates
                .iter()
                .map(|candidate| {
                    let marker = if self.selected_workers.contains(&candidate.session_id) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    format!("{marker} WORKER {}", candidate.label)
                })
                .collect(),
            ObserverManagerScreen::GroupActions => vec![
                "Open Observer".to_owned(),
                "Edit workers".to_owned(),
                "Change orchestrator".to_owned(),
                "Delete group".to_owned(),
            ],
            ObserverManagerScreen::ReviewTopology => self.review_labels(),
            ObserverManagerScreen::ConfirmDelete => vec![format!(
                "Metadata only; workloads keep running. Delete {}?",
                self.selected_group_title()
            )],
        }
    }

    pub fn context_text(&self) -> Option<String> {
        match self.screen {
            ObserverManagerScreen::ReviewTopology
                if self.review_kind == Some(ReviewKind::ReassignOrchestrator) =>
            {
                None
            }
            ObserverManagerScreen::CreateWorkers | ObserverManagerScreen::ReviewTopology
                if self.selected_group.is_none() =>
            {
                let orchestrator = self.selected_orchestrator?;
                let label = self
                    .eligible
                    .iter()
                    .find(|candidate| candidate.session_id == orchestrator)?
                    .label
                    .clone();
                Some(format!("ORCHESTRATOR {label}"))
            }
            ObserverManagerScreen::GroupActions
            | ObserverManagerScreen::EditWorkers
            | ObserverManagerScreen::ChangeOrchestrator
            | ObserverManagerScreen::ReviewTopology => Some(self.selected_group_summary()),
            _ => None,
        }
    }

    pub fn footer_text(&self) -> String {
        let guidance = match self.screen {
            ObserverManagerScreen::Groups => {
                "n Create · Enter manage · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::CreateOrchestrator => {
                "Enter choose orchestrator · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::CreateWorkers => {
                "Space select workers · Enter review · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::GroupActions => {
                "Enter choose · e Edit · d Delete · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::EditWorkers => {
                "Space add/remove · Enter review · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::ChangeOrchestrator => {
                "Enter review replacement · ↑/↓ navigate · Esc/Backspace back"
            }
            ObserverManagerScreen::ReviewTopology => "Enter apply once · Esc/Backspace revise",
            ObserverManagerScreen::ConfirmDelete => {
                "y delete metadata · n/Esc keep · workloads are untouched"
            }
        };
        self.notice.as_ref().map_or_else(
            || guidance.to_owned(),
            |notice| format!("{notice} · {guidance}"),
        )
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn handle(&mut self, event: ObserverManagerEvent) -> ObserverManagerOutcome {
        if self.screen == ObserverManagerScreen::ConfirmDelete {
            return match event {
                ObserverManagerEvent::ConfirmDelete => self.delete_action(),
                ObserverManagerEvent::DismissDelete | ObserverManagerEvent::Back => {
                    self.screen = ObserverManagerScreen::GroupActions;
                    self.selected_index = 3;
                    ObserverManagerOutcome::Continue
                }
                _ => ObserverManagerOutcome::Continue,
            };
        }
        match event {
            ObserverManagerEvent::Previous => {
                self.move_selection(-1);
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Next => {
                self.move_selection(1);
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Back => self.back(),
            ObserverManagerEvent::Toggle => {
                self.toggle_worker();
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Create if self.screen == ObserverManagerScreen::Groups => {
                self.begin_create();
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Edit if self.screen == ObserverManagerScreen::GroupActions => {
                self.begin_edit();
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Delete if self.screen == ObserverManagerScreen::GroupActions => {
                self.screen = ObserverManagerScreen::ConfirmDelete;
                self.selected_index = 0;
                ObserverManagerOutcome::Continue
            }
            ObserverManagerEvent::Confirm => self.confirm(),
            _ => ObserverManagerOutcome::Continue,
        }
    }

    fn confirm(&mut self) -> ObserverManagerOutcome {
        match self.screen {
            ObserverManagerScreen::Groups => {
                if self.selected_index < self.groups.len() {
                    self.selected_group = Some(self.selected_index);
                    self.screen = ObserverManagerScreen::GroupActions;
                    self.selected_index = 0;
                } else {
                    self.begin_create();
                }
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::CreateOrchestrator => {
                let Some(candidate) = self.screen_candidates.get(self.selected_index) else {
                    self.notice = Some("No running exact-owned workloads are available".to_owned());
                    return ObserverManagerOutcome::Continue;
                };
                self.selected_orchestrator = Some(candidate.session_id);
                self.screen_candidates = self
                    .eligible
                    .iter()
                    .filter(|worker| worker.session_id != candidate.session_id)
                    .cloned()
                    .collect();
                self.selected_workers.clear();
                self.screen = ObserverManagerScreen::CreateWorkers;
                self.selected_index = 0;
                self.notice = None;
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::CreateWorkers => self.begin_review(ReviewKind::Create),
            ObserverManagerScreen::GroupActions => match self.selected_index {
                0 => ObserverManagerOutcome::Action(ObserverManagerAction::Launch {
                    group_id: self.selected_group_id(),
                }),
                1 => {
                    self.begin_edit();
                    ObserverManagerOutcome::Continue
                }
                2 => {
                    self.begin_reassign();
                    ObserverManagerOutcome::Continue
                }
                3 => {
                    self.screen = ObserverManagerScreen::ConfirmDelete;
                    self.selected_index = 0;
                    ObserverManagerOutcome::Continue
                }
                _ => ObserverManagerOutcome::Continue,
            },
            ObserverManagerScreen::EditWorkers => self.begin_review(ReviewKind::ReplaceWorkers),
            ObserverManagerScreen::ChangeOrchestrator => {
                let Some(candidate) = self.screen_candidates.get(self.selected_index) else {
                    return ObserverManagerOutcome::Continue;
                };
                self.selected_orchestrator = Some(candidate.session_id);
                self.begin_review(ReviewKind::ReassignOrchestrator)
            }
            ObserverManagerScreen::ReviewTopology => self.review_action(),
            ObserverManagerScreen::ConfirmDelete => ObserverManagerOutcome::Continue,
        }
    }

    fn begin_create(&mut self) {
        self.notice = match self.eligible.len() {
            0 => Some("No running exact-owned workloads are available".to_owned()),
            1 => Some("Observer needs at least two running exact-owned workloads".to_owned()),
            _ => None,
        };
        if self.notice.is_some() {
            self.screen = ObserverManagerScreen::Groups;
            self.selected_index = self.groups.len();
            return;
        }
        self.screen = ObserverManagerScreen::CreateOrchestrator;
        self.screen_candidates.clone_from(&self.eligible);
        self.selected_index = 0;
        self.selected_group = None;
        self.selected_orchestrator = None;
        self.selected_workers.clear();
    }

    fn begin_edit(&mut self) {
        let group = self.selected_group().clone();
        let mut candidates = Vec::with_capacity(self.eligible.len() + group.workers.len());
        let mut known = HashSet::with_capacity(group.workers.len());
        for (index, member) in group.workers.iter().enumerate() {
            let mut candidate = self
                .eligible
                .iter()
                .find(|candidate| candidate.session_id == member.session_id)
                .cloned()
                .unwrap_or_else(|| {
                    let label = member
                        .title
                        .as_ref()
                        .map(|title| title.as_str().to_owned())
                        .unwrap_or_else(|| format!("Unavailable worker {}", index + 1));
                    Candidate {
                        session_id: member.session_id,
                        label: safe_component(&label, 72),
                        host: "unavailable".to_owned(),
                        repository: format!("worker-{}", index + 1),
                        title: safe_component(&label, 96)
                            .parse()
                            .expect("safe member title"),
                        existing: None,
                    }
                });
            candidate.existing = Some(member.clone());
            known.insert(member.session_id);
            candidates.push(candidate);
        }
        candidates.extend(
            self.eligible
                .iter()
                .filter(|candidate| {
                    candidate.session_id != group.orchestrator_session_id
                        && !known.contains(&candidate.session_id)
                })
                .cloned(),
        );
        self.selected_workers = group
            .workers
            .iter()
            .map(|worker| worker.session_id)
            .collect();
        self.screen_candidates = candidates;
        self.screen = ObserverManagerScreen::EditWorkers;
        self.selected_index = 0;
        self.notice = None;
    }

    fn begin_reassign(&mut self) {
        let current = self.selected_group().orchestrator_session_id;
        self.screen_candidates = self
            .eligible
            .iter()
            .filter(|candidate| candidate.session_id != current)
            .cloned()
            .collect();
        self.selected_orchestrator = None;
        self.screen = ObserverManagerScreen::ChangeOrchestrator;
        self.selected_index = 0;
        self.notice = if self.screen_candidates.is_empty() {
            Some("No other running exact-owned workload is available".to_owned())
        } else {
            None
        };
    }

    fn begin_review(&mut self, kind: ReviewKind) -> ObserverManagerOutcome {
        if matches!(kind, ReviewKind::Create | ReviewKind::ReplaceWorkers)
            && self.selected_workers.is_empty()
        {
            self.notice = Some("Select at least one worker".to_owned());
            return ObserverManagerOutcome::Continue;
        }
        self.review_kind = Some(kind);
        self.review_return_index = self.selected_index;
        self.screen = ObserverManagerScreen::ReviewTopology;
        self.selected_index = 0;
        self.notice = None;
        ObserverManagerOutcome::Continue
    }

    fn review_action(&self) -> ObserverManagerOutcome {
        match self.review_kind.expect("review screen has a mutation") {
            ReviewKind::Create => self.create_action(),
            ReviewKind::ReplaceWorkers => self.replace_action(),
            ReviewKind::ReassignOrchestrator => {
                ObserverManagerOutcome::Action(ObserverManagerAction::ReassignOrchestrator {
                    expected_group: self.selected_group().clone(),
                    orchestrator_session_id: self
                        .selected_orchestrator
                        .expect("reassign review has a replacement"),
                })
            }
        }
    }

    fn create_action(&self) -> ObserverManagerOutcome {
        let orchestrator = self
            .selected_orchestrator
            .expect("create review has an orchestrator");
        let candidate = self
            .eligible
            .iter()
            .find(|candidate| candidate.session_id == orchestrator)
            .expect("selected orchestrator remains eligible");
        let (id, title) = self.next_group_identity(candidate);
        ObserverManagerOutcome::Action(ObserverManagerAction::Create {
            id,
            title,
            orchestrator_session_id: orchestrator,
            workers: self.selected_worker_specs(),
        })
    }

    fn replace_action(&self) -> ObserverManagerOutcome {
        ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers {
            expected_group: self.selected_group().clone(),
            workers: self.selected_worker_specs(),
        })
    }

    fn delete_action(&self) -> ObserverManagerOutcome {
        ObserverManagerOutcome::Action(ObserverManagerAction::Delete {
            expected_group: self.selected_group().clone(),
        })
    }

    fn selected_worker_specs(&self) -> Vec<OrchestrationWorkerSpec> {
        self.screen_candidates
            .iter()
            .filter(|candidate| self.selected_workers.contains(&candidate.session_id))
            .map(|candidate| {
                candidate.existing.as_ref().map_or_else(
                    || OrchestrationWorkerSpec {
                        session_id: candidate.session_id,
                        title: Some(candidate.title.clone()),
                        capabilities: DEFAULT_CAPABILITIES,
                    },
                    |existing| OrchestrationWorkerSpec {
                        session_id: existing.session_id,
                        title: existing.title.clone(),
                        capabilities: existing.capabilities,
                    },
                )
            })
            .collect()
    }

    fn next_group_identity(
        &self,
        orchestrator: &Candidate,
    ) -> (OrchestrationGroupId, OrchestrationTitle) {
        let base = format!(
            "observer-{}-{}",
            slug(&orchestrator.host, 24),
            slug(&orchestrator.repository, 24)
        );
        let existing = self
            .groups
            .iter()
            .map(|group| group.id.as_str())
            .collect::<HashSet<_>>();
        let mut suffix = 1usize;
        loop {
            let id = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            if !existing.contains(id.as_str()) {
                let title = if suffix == 1 {
                    format!("Observer {} {}", orchestrator.host, orchestrator.repository)
                } else {
                    format!(
                        "Observer {} {} {suffix}",
                        orchestrator.host, orchestrator.repository
                    )
                };
                return (
                    id.parse().expect("generated group id is safe"),
                    safe_component(&title, OrchestrationTitle::MAX_BYTES)
                        .parse()
                        .expect("generated group title is safe"),
                );
            }
            suffix += 1;
        }
    }

    fn toggle_worker(&mut self) {
        if !matches!(
            self.screen,
            ObserverManagerScreen::CreateWorkers | ObserverManagerScreen::EditWorkers
        ) {
            return;
        }
        let Some(candidate) = self.screen_candidates.get(self.selected_index) else {
            return;
        };
        if !self.selected_workers.remove(&candidate.session_id) {
            self.selected_workers.insert(candidate.session_id);
        }
        self.notice = None;
    }

    fn move_selection(&mut self, offset: isize) {
        let length = self.item_labels().len();
        if length == 0 {
            self.selected_index = 0;
            return;
        }
        self.selected_index = if offset < 0 {
            self.selected_index.checked_sub(1).unwrap_or(length - 1)
        } else {
            (self.selected_index + 1) % length
        };
    }

    fn back(&mut self) -> ObserverManagerOutcome {
        self.notice = None;
        match self.screen {
            ObserverManagerScreen::Groups => {
                ObserverManagerOutcome::Action(ObserverManagerAction::BackToPicker)
            }
            ObserverManagerScreen::CreateOrchestrator | ObserverManagerScreen::GroupActions => {
                self.screen = ObserverManagerScreen::Groups;
                self.selected_index = self.selected_group.unwrap_or(0).min(self.groups.len());
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::CreateWorkers => {
                self.screen = ObserverManagerScreen::CreateOrchestrator;
                self.screen_candidates.clone_from(&self.eligible);
                self.selected_index = 0;
                self.selected_workers.clear();
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::EditWorkers => {
                self.screen = ObserverManagerScreen::GroupActions;
                self.selected_index = 1;
                self.selected_workers.clear();
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::ChangeOrchestrator => {
                self.screen = ObserverManagerScreen::GroupActions;
                self.selected_index = 2;
                self.selected_orchestrator = None;
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::ReviewTopology => {
                self.screen = match self.review_kind.expect("review has a mutation") {
                    ReviewKind::Create => ObserverManagerScreen::CreateWorkers,
                    ReviewKind::ReplaceWorkers => ObserverManagerScreen::EditWorkers,
                    ReviewKind::ReassignOrchestrator => ObserverManagerScreen::ChangeOrchestrator,
                };
                self.selected_index = self.review_return_index;
                self.review_kind = None;
                ObserverManagerOutcome::Continue
            }
            ObserverManagerScreen::ConfirmDelete => unreachable!(),
        }
    }

    fn selected_group(&self) -> &OrchestrationGroup {
        &self.groups[self
            .selected_group
            .expect("group action has a selected group")]
    }

    fn selected_group_id(&self) -> OrchestrationGroupId {
        self.selected_group().id.clone()
    }

    fn selected_group_title(&self) -> &str {
        self.selected_group().title.as_str()
    }

    fn selected_group_summary(&self) -> String {
        self.group_summary(self.selected_group())
    }

    fn group_summary(&self, group: &OrchestrationGroup) -> String {
        let orchestrator_label = self
            .session_labels
            .get(&group.orchestrator_session_id)
            .map(String::as_str)
            .unwrap_or("unavailable");
        let orchestrator_health = if self
            .running_sessions
            .contains(&group.orchestrator_session_id)
        {
            ""
        } else {
            " unavailable"
        };
        let unavailable = group
            .workers
            .iter()
            .filter(|worker| !self.running_sessions.contains(&worker.session_id))
            .count();
        format!(
            "{} · ORCHESTRATOR {}{} · {} workers · {} unavailable",
            group.title.as_str(),
            orchestrator_label,
            orchestrator_health,
            group.workers.len(),
            unavailable
        )
    }

    fn review_labels(&self) -> Vec<String> {
        let orchestrator = self
            .selected_orchestrator
            .or_else(|| {
                self.selected_group
                    .map(|_| self.selected_group().orchestrator_session_id)
            })
            .expect("review has an orchestrator");
        let orchestrator_label = self
            .eligible
            .iter()
            .find(|candidate| candidate.session_id == orchestrator)
            .map(|candidate| candidate.label.as_str())
            .or_else(|| self.session_labels.get(&orchestrator).map(String::as_str))
            .unwrap_or("unavailable");
        let mut labels = vec![format!("ORCHESTRATOR {orchestrator_label}")];
        if self.review_kind == Some(ReviewKind::ReassignOrchestrator)
            && self
                .selected_group()
                .workers
                .iter()
                .any(|worker| worker.session_id == orchestrator)
        {
            labels.push("Promote selected WORKER; remove from workers".to_owned());
        }
        let workers: Vec<(SessionId, String)> = match self.review_kind {
            Some(ReviewKind::ReassignOrchestrator) => self
                .selected_group()
                .workers
                .iter()
                .filter(|worker| worker.session_id != orchestrator)
                .map(|worker| {
                    let label = worker
                        .title
                        .as_ref()
                        .map(|title| title.as_str().to_owned())
                        .or_else(|| self.session_labels.get(&worker.session_id).cloned())
                        .unwrap_or_else(|| "unavailable".to_owned());
                    (worker.session_id, label)
                })
                .collect(),
            _ => self
                .screen_candidates
                .iter()
                .filter(|candidate| self.selected_workers.contains(&candidate.session_id))
                .map(|candidate| (candidate.session_id, candidate.label.clone()))
                .collect(),
        };
        labels.extend(
            workers
                .into_iter()
                .map(|(_, label)| format!("WORKER {label}")),
        );
        labels
    }
}

fn is_eligible(record: &SessionRecord) -> bool {
    record.status == SessionStatus::Running
        && record.ownership_proof.is_some()
        && record.tmux_session_id.is_some()
}

fn candidate_labels(records: &[SessionRecord]) -> HashMap<SessionId, String> {
    let mut groups = HashMap::<String, Vec<SessionId>>::new();
    for record in records.iter().filter(|record| is_eligible(record)) {
        groups
            .entry(candidate_label(record))
            .or_default()
            .push(record.id);
    }
    let mut labels = HashMap::new();
    for (base, ids) in groups {
        if ids.len() == 1 {
            labels.insert(ids[0], base);
            continue;
        }
        let mut width = SessionId::SHORT_REFERENCE_WIDTH;
        loop {
            let tokens = ids
                .iter()
                .map(|id| id.reference_token(width))
                .collect::<HashSet<_>>();
            if tokens.len() == ids.len() || width == SessionId::MAX_REFERENCE_WIDTH {
                break;
            }
            width += 1;
        }
        for id in ids {
            labels.insert(id, format!("{base} [{}]", id.reference_token(width)));
        }
    }
    labels
}

fn candidate(record: &SessionRecord, label: &str) -> Result<Candidate> {
    let host = safe_component(&record.host, 24);
    let repository = safe_component(repository_name(&record.directory), 32);
    Ok(Candidate {
        session_id: record.id,
        label: label.to_owned(),
        host,
        repository,
        title: safe_component(label, 96)
            .parse()
            .context("generate safe Observer member title")?,
        existing: None,
    })
}

fn candidate_label(record: &SessionRecord) -> String {
    format!(
        "{} / {} / {}",
        safe_component(&record.host, 24),
        safe_component(repository_name(&record.directory), 32),
        safe_component(record.preset.as_deref().unwrap_or("shell"), 24)
    )
}

fn repository_name(directory: &str) -> &str {
    directory
        .trim_end_matches('/')
        .rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or("workspace")
}

fn safe_component(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    let mut pending_space = false;
    for byte in input.bytes() {
        let character = if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            Some(char::from(byte))
        } else {
            None
        };
        if let Some(character) = character {
            if pending_space && !output.is_empty() && output.len() < max_bytes {
                output.push(' ');
            }
            pending_space = false;
            if output.len() < max_bytes {
                output.push(character);
            }
        } else {
            pending_space = true;
        }
        if output.len() >= max_bytes {
            break;
        }
    }
    while output.ends_with([' ', '-', '_', '.']) {
        output.pop();
    }
    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

fn slug(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    let mut separator = false;
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() {
            if separator && !output.is_empty() && output.len() < max_bytes {
                output.push('-');
            }
            separator = false;
            if output.len() < max_bytes {
                output.push(char::from(byte.to_ascii_lowercase()));
            }
        } else {
            separator = true;
        }
        if output.len() >= max_bytes {
            break;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        return "group".to_owned();
    }
    if !output.as_bytes()[0].is_ascii_lowercase() {
        output.truncate(max_bytes.saturating_sub("group-".len()));
        while output.ends_with('-') {
            output.pop();
        }
        return format!("group-{output}");
    }
    output
}

pub fn run_observer_manager(mut state: ObserverManagerState) -> Result<ObserverManagerAction> {
    enable_raw_mode().context("enable Observer manager raw mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("enter Observer manager alternate screen");
    }
    let result = run_terminal_manager(&mut state);
    let screen = execute!(io::stdout(), LeaveAlternateScreen, Show)
        .context("restore Observer manager screen");
    let raw = disable_raw_mode().context("disable Observer manager raw mode");
    match (result, screen, raw) {
        (Ok(action), Ok(()), Ok(())) => Ok(action),
        (result, screen, raw) => {
            let mut failures = Vec::new();
            if let Err(error) = result {
                failures.push(format!("manager: {error:#}"));
            }
            if let Err(error) = screen {
                failures.push(format!("screen cleanup: {error:#}"));
            }
            if let Err(error) = raw {
                failures.push(format!("terminal cleanup: {error:#}"));
            }
            Err(anyhow::anyhow!(failures.join("; ")))
        }
    }
}

fn run_terminal_manager(state: &mut ObserverManagerState) -> Result<ObserverManagerAction> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize Observer manager")?;
    terminal.clear().context("clear Observer manager")?;
    loop {
        terminal
            .draw(|frame| render(frame, frame.area(), state))
            .context("draw Observer manager")?;
        if !event::poll(std::time::Duration::from_millis(50))
            .context("poll Observer manager input")?
        {
            continue;
        }
        let Event::Key(key) = event::read().context("read Observer manager input")? else {
            continue;
        };
        let Some(event) = event_for_key(key, state.screen) else {
            continue;
        };
        if let ObserverManagerOutcome::Action(action) = state.handle(event) {
            return Ok(action);
        }
    }
}

fn event_for_key(key: KeyEvent, screen: ObserverManagerScreen) -> Option<ObserverManagerEvent> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return Some(ObserverManagerEvent::Back);
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    if screen == ObserverManagerScreen::ConfirmDelete {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        return match key.code {
            KeyCode::Char('y' | 'Y') => Some(ObserverManagerEvent::ConfirmDelete),
            KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Backspace => {
                Some(ObserverManagerEvent::DismissDelete)
            }
            _ => None,
        };
    }
    match key.code {
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Some(ObserverManagerEvent::Previous),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Some(ObserverManagerEvent::Next),
        KeyCode::Esc | KeyCode::Backspace => Some(ObserverManagerEvent::Back),
        KeyCode::Enter if key.kind == KeyEventKind::Press => Some(ObserverManagerEvent::Confirm),
        KeyCode::Char(' ') if key.kind == KeyEventKind::Press => Some(ObserverManagerEvent::Toggle),
        KeyCode::Char('n' | 'N') if key.kind == KeyEventKind::Press => {
            Some(ObserverManagerEvent::Create)
        }
        KeyCode::Char('e' | 'E') if key.kind == KeyEventKind::Press => {
            Some(ObserverManagerEvent::Edit)
        }
        KeyCode::Char('d' | 'D') if key.kind == KeyEventKind::Press => {
            Some(ObserverManagerEvent::Delete)
        }
        _ => None,
    }
}

fn manager_style(selected: bool) -> Style {
    let style = Style::default().fg(Color::Reset).bg(Color::Reset);
    if selected {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn render(frame: &mut Frame<'_>, area: Rect, state: &ObserverManagerState) {
    if area.is_empty() {
        return;
    }
    if area.width < MIN_MANAGER_WIDTH || area.height < MIN_MANAGER_HEIGHT {
        frame.render_widget(Block::default().style(manager_style(false)), area);
        frame.render_widget(
            Paragraph::new(MANAGER_RESIZE_MESSAGE)
                .style(manager_style(false))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    frame.render_widget(Block::default().style(manager_style(false)), area);
    let labels = state.item_labels();
    let visible_rows = labels.len().clamp(1, MAX_VISIBLE_ROWS) as u16;
    let panel_width = area.width.min(MAX_PANEL_WIDTH);
    let footer = state.footer_text();
    let context = state.context_text();
    let footer_width = panel_width.saturating_sub(4).max(1);
    let context_height = context
        .as_deref()
        .map_or(0, |text| wrapped_line_count(text, footer_width));
    let footer_height = wrapped_line_count(&footer, footer_width);
    let panel_height = visible_rows
        .saturating_add(context_height)
        .saturating_add(footer_height)
        .saturating_add(4)
        .min(area.height);
    let panel = Rect::new(
        area.x
            .saturating_add(area.width.saturating_sub(panel_width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(panel_height) / 2),
        panel_width,
        panel_height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::default()
            .title(format!(" Tether · {} ", state.frame_title()))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .style(manager_style(false)),
        panel,
    );
    let inner = panel.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(context_height),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(inner);
    if let Some(context) = context {
        frame.render_widget(
            Paragraph::new(context)
                .style(manager_style(false))
                .wrap(Wrap { trim: true }),
            chunks[0],
        );
    }
    let list_height = chunks[1].height.saturating_sub(1);
    let max_rows = usize::from(list_height);
    let (start, metadata) = viewport_metadata(state.selected_index(), labels.len(), max_rows);
    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(chunks[1]);
    frame.render_widget(
        Paragraph::new(metadata).style(manager_style(false)),
        list_chunks[0],
    );
    let lines = labels
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .map(|(index, label)| {
            let label = bounded_label(label, usize::from(list_chunks[1].width.saturating_sub(2)));
            let selected = index == state.selected_index() && !labels.is_empty();
            Line::from(vec![
                Span::styled(if selected { "> " } else { "  " }, manager_style(selected)),
                Span::styled(label, manager_style(selected)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).style(manager_style(false)),
        list_chunks[1],
    );
    frame.render_widget(
        Paragraph::new(footer)
            .style(manager_style(false))
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
}

fn viewport_metadata(selected: usize, len: usize, rows: usize) -> (usize, String) {
    if len == 0 {
        return (0, "0/0".to_owned());
    }
    let selected = selected.min(len - 1);
    let rows = rows.max(1).min(len);
    let start = selected
        .saturating_sub(rows / 2)
        .min(len.saturating_sub(rows));
    let end = start.saturating_add(rows).min(len);
    let mut metadata = format!("{}/{}", selected + 1, len);
    if start > 0 {
        metadata.push_str(" · more above");
    }
    if end < len {
        metadata.push_str(" · more below");
    }
    (start, metadata)
}

fn bounded_label(text: &str, max_width: usize) -> String {
    if Line::from(text).width() <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(1);
    let mut end = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        let next = index + grapheme.len();
        if Line::from(&text[..next]).width() > content_width {
            break;
        }
        end = next;
    }
    let mut bounded = String::with_capacity(end.saturating_add('…'.len_utf8()));
    bounded.push_str(&text[..end]);
    bounded.push('…');
    bounded
}

#[derive(Debug)]
pub enum ObserverManagerRenderError {}

impl fmt::Display for ObserverManagerRenderError {
    fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl std::error::Error for ObserverManagerRenderError {}

pub fn render_to_text(
    width: u16,
    height: u16,
    state: &ObserverManagerState,
) -> Result<String, ObserverManagerRenderError> {
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).map_err(|error| match error {})?;
    terminal
        .draw(|frame| render(frame, frame.area(), state))
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
pub fn render_to_styles(
    width: u16,
    height: u16,
    state: &ObserverManagerState,
) -> Result<Vec<(Color, Color, Modifier)>, ObserverManagerRenderError> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).map_err(|error| match error {})?;
    terminal
        .draw(|frame| render(frame, frame.area(), state))
        .map_err(|error| match error {})?;
    Ok(terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| (cell.fg, cell.bg, cell.modifier))
        .collect())
}

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| Line::from(line).width().max(1).div_ceil(width))
        .sum::<usize>()
        .clamp(1, usize::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_keeps_mutations_press_only() {
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char(' '), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(KeyCode::Char('d'), KeyModifiers::NONE, KeyEventKind::Repeat),
        ] {
            assert_eq!(event_for_key(key, ObserverManagerScreen::EditWorkers), None);
        }
    }

    #[test]
    fn manager_label_truncation_preserves_extended_graphemes() {
        let cases = [
            ("adjacent flags", "🇺🇸", "🇨🇦xxxx"),
            ("keycap", "1\u{fe0f}\u{20e3}", "xxxx"),
            ("combining mark", "e\u{301}", "xxxx"),
            ("text variation selector", "❤\u{fe0e}", "xxxx"),
            ("emoji variation selector", "❤\u{fe0f}", "xxxx"),
            ("emoji ZWJ sequence", "👩\u{200d}💻", "xxxx"),
        ];

        for (name, cluster, tail) in cases {
            let prefix = "aaaaaaaa";
            let boundary = format!("{prefix}{cluster}");
            let max_width = Line::from(boundary.as_str()).width() + 1;
            assert_eq!(
                bounded_label(&format!("{boundary}{tail}"), max_width),
                format!("{boundary}…"),
                "{name}"
            );
        }
    }

    #[test]
    fn manager_chrome_uses_terminal_default_colors_with_bold_selection() {
        let manager = ObserverManagerState::from_state(&State::default(), None).unwrap();
        let styles = render_to_styles(60, 12, &manager).unwrap();
        assert!(styles.iter().all(|(foreground, background, _)| {
            *foreground == Color::Reset && *background == Color::Reset
        }));
        assert!(
            styles
                .iter()
                .any(|(_, _, modifier)| modifier.contains(Modifier::BOLD))
        );
    }
}
