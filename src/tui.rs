use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
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
    config::Config,
    discovery::{
        DiscoveryCompletion, DiscoveryLocation, DiscoveryMessage, DiscoveryRequest, DiscoveryRun,
        DiscoveryService,
    },
    lifecycle::{CloseOwnedError, LifecycleService, PrunePreview, PruneService},
    model::{ExternalSessionName, Placement, SessionId},
    state::{SessionRecord, SessionStatus, State, compare_normal_sessions, is_normal_session},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, StatusHost, StatusMessage,
        StatusRequest, StatusRun, StatusService, WorkloadStatus,
    },
};

const MIN_PICKER_WIDTH: u16 = 40;
const MIN_PICKER_HEIGHT: u16 = 8;
const NARROW_GUIDANCE_WIDTH: u16 = 32;
const PICKER_RESIZE_MESSAGE: &str = "Resize terminal to at least 40x8";

const SHELL_COMMAND: &str = "exec ${SHELL:-/bin/sh}";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerCommand {
    Shell,
    Preset { name: String, command: String },
}

impl PickerCommand {
    pub fn label(&self) -> &str {
        match self {
            Self::Shell => "Shell",
            Self::Preset { name, .. } => name,
        }
    }

    fn selection_parts(&self) -> (Option<String>, String) {
        match self {
            Self::Shell => (None, SHELL_COMMAND.to_owned()),
            Self::Preset { name, command } => (Some(name.clone()), command.clone()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerHostOrigin {
    Effective,
    Retained,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerWorkload {
    pub id: SessionId,
    pub status: SessionStatus,
    pub legacy: bool,
    pub last_used_at: chrono::DateTime<chrono::Utc>,
    pub base_label: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PickerExternalSession {
    name: ExternalSessionName,
    label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerHost {
    pub name: String,
    pub label: String,
    pub target: Option<String>,
    pub origin: PickerHostOrigin,
    pub directories: Vec<String>,
    pub scan_roots: Vec<String>,
    pub commands: Vec<PickerCommand>,
    pub workloads: Vec<PickerWorkload>,
    pub allow_existing: bool,
    pub allow_create: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerOptions {
    pub hosts: Vec<PickerHost>,
    pub default_placement: Placement,
}

impl PickerOptions {
    /// Builds effective hosts in configuration precedence order, followed by
    /// retained exact `(host, target)` state groups in deterministic order.
    pub fn from_config_state(
        config: &Config,
        state: &State,
        local_directory: &str,
        include_local: bool,
    ) -> Self {
        let mut hosts = Vec::with_capacity(config.hosts.len() + usize::from(include_local));
        let mut host_names = HashSet::with_capacity(hosts.capacity());
        let mut effective_keys = HashSet::with_capacity(hosts.capacity());

        if include_local {
            host_names.insert("local".to_owned());
            effective_keys.insert(("local".to_owned(), "local".to_owned()));
            let mut scan_roots = config
                .discovery
                .local_roots
                .iter()
                .map(|root| expand_local_root(root, local_directory))
                .collect::<Vec<_>>();
            if scan_roots.is_empty() {
                scan_roots.push(local_directory.to_owned());
            }
            let mut directories = recent_directories(state, "local", "local");
            for root in &scan_roots {
                push_unique(&mut directories, root);
            }
            hosts.push(PickerHost {
                name: "local".to_owned(),
                label: "local".to_owned(),
                target: None,
                origin: PickerHostOrigin::Effective,
                directories,
                scan_roots,
                commands: vec![PickerCommand::Shell],
                workloads: owned_workloads(state, "local", "local"),
                allow_existing: true,
                allow_create: true,
            });
        }

        for host in &config.hosts {
            if !host_names.insert(host.name.clone()) {
                continue;
            }
            effective_keys.insert((host.name.clone(), host.target.clone()));
            let mut scan_roots = host.roots.clone();
            if scan_roots.is_empty() {
                scan_roots.push("~".to_owned());
            }
            let mut directories = recent_directories(state, &host.name, &host.target);
            for root in &scan_roots {
                push_unique(&mut directories, root);
            }
            let mut commands = Vec::with_capacity(host.presets.len() + 1);
            commands.push(PickerCommand::Shell);
            commands.extend(host.presets.iter().map(|preset| PickerCommand::Preset {
                name: preset.name.clone(),
                command: preset.command.clone(),
            }));
            hosts.push(PickerHost {
                name: host.name.clone(),
                label: host.name.clone(),
                target: Some(host.target.clone()),
                origin: PickerHostOrigin::Effective,
                directories,
                scan_roots,
                commands,
                workloads: owned_workloads(state, &host.name, &host.target),
                allow_existing: true,
                allow_create: true,
            });
        }

        let mut retained = BTreeMap::<(String, String), ()>::new();
        for record in state
            .sessions
            .iter()
            .filter(|record| is_normal_session(record))
        {
            let key = (record.host.clone(), record.target.clone());
            if !effective_keys.contains(&key) {
                retained.insert(key, ());
            }
        }
        for ((name, target), ()) in retained {
            let directories = recent_directories(state, &name, &target);
            hosts.push(PickerHost {
                label: format!("{name} · retained · {target}"),
                workloads: owned_workloads(state, &name, &target),
                name,
                target: (target != "local").then_some(target),
                origin: PickerHostOrigin::Retained,
                directories,
                scan_roots: Vec::new(),
                commands: vec![PickerCommand::Shell],
                allow_existing: false,
                allow_create: false,
            });
        }

        Self {
            hosts,
            default_placement: config.ui.placement,
        }
    }
}

fn expand_local_root(root: &str, home: &str) -> String {
    if root == "~" {
        return home.to_owned();
    }
    if let Some(rest) = root.strip_prefix("~/") {
        return PathBuf::from(home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    root.to_owned()
}

fn recent_directories(state: &State, host: &str, target: &str) -> Vec<String> {
    let mut sessions: Vec<&SessionRecord> = state
        .sessions
        .iter()
        .filter(|session| {
            session.host == host && session.target == target && is_normal_session(session)
        })
        .collect();
    sessions.sort_by(|left, right| {
        compare_normal_sessions(
            left.status,
            left.last_used_at,
            (left.directory.as_str(), left.id),
            right.status,
            right.last_used_at,
            (right.directory.as_str(), right.id),
        )
    });
    let mut directories = Vec::with_capacity(sessions.len());
    for session in sessions {
        push_unique(&mut directories, &session.directory);
    }
    directories
}
fn owned_workloads(state: &State, host: &str, target: &str) -> Vec<PickerWorkload> {
    let mut sessions: Vec<&SessionRecord> = state
        .sessions
        .iter()
        .filter(|session| {
            session.host == host && session.target == target && is_normal_session(session)
        })
        .collect();
    sessions.sort_by(|left, right| {
        compare_normal_sessions(
            left.status,
            left.last_used_at,
            left.id,
            right.status,
            right.last_used_at,
            right.id,
        )
    });

    sessions
        .into_iter()
        .map(|session| {
            let label = workload_label(session);
            PickerWorkload {
                id: session.id,
                status: session.status,
                legacy: session.ownership_proof.is_none(),
                last_used_at: session.last_used_at,
                base_label: label.clone(),
                label,
            }
        })
        .collect()
}

fn workload_label(session: &SessionRecord) -> String {
    let command = session.preset.as_deref().unwrap_or("Shell");
    let id = session.id.to_string();
    let short_id = &id[id.len().saturating_sub(8)..];
    if session.ownership_proof.is_none() && session.status != SessionStatus::Removed {
        return format!(
            "[legacy] Tether · Remove metadata …{} · {} · {}",
            short_id, command, session.directory
        );
    }
    let (lifecycle, action) = match session.status {
        SessionStatus::Creating => ("creating", "Pending"),
        SessionStatus::Running => ("running", "Open"),
        SessionStatus::Stopping => ("stopping", "Pending"),
        SessionStatus::Ended => ("ended", "Restart"),
        SessionStatus::Removed => ("removed", "Metadata"),
    };
    format!(
        "[{lifecycle}] Tether · {action} …{} · {} · {}",
        short_id, command, session.directory
    )
}

fn push_unique(values: &mut Vec<String>, candidate: &str) {
    if !candidate.is_empty() && !values.iter().any(|value| value == candidate) {
        values.push(candidate.to_owned());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSelection {
    pub host: String,
    pub directory: String,
    pub preset: Option<String>,
    pub command: String,
    pub placement: Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerCloseAction {
    Stop,
    Remove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerSelection {
    Create(OpenSelection),
    Resume {
        id: SessionId,
        placement: Placement,
    },
    Restart {
        id: SessionId,
        placement: Placement,
    },
    AttachExternal {
        host: String,
        target: Option<String>,
        name: ExternalSessionName,
        placement: Placement,
    },
    ManageObservers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerStage {
    Host,
    Resource,
    Directory,
    Command,
    Placement,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerEvent {
    Previous,
    Next,
    Confirm,
    Back,
    Cancel,
    Refresh,
    ManageObservers,
    Close,
    ConfirmClose,
    DismissClose,
    BeginPrune,
    ConfirmPrune,
    DismissPrune,
    BeginFilter,
    BeginPath,
    Insert(char),
    Delete,
    SubmitInput,
    ExitInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerOutcome {
    Continue,
    RefreshRequested,
    CloseOwnedRequested {
        id: SessionId,
        generation: u64,
        action: PickerCloseAction,
    },
    PrunePreviewRequested {
        older_than_days: u64,
        generation: u64,
    },
    PruneApplyRequested {
        preview: PrunePreview,
        generation: u64,
    },
    Selected(PickerSelection),
    Cancelled,
}

/// A failed create, resume, attach, or placement attempt retained by the picker.
///
/// Integrations should pass the exact attempted selection back with a
/// user-safe diagnostic. Confirming retries that same selection; cancelling
/// dismisses the error without executing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerOperationError {
    pub attempted: PickerSelection,
    pub diagnostic: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerInput {
    None,
    Filter(String),
    DirectPath(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerCloseModal {
    Confirm { id: SessionId },
    Failed { id: SessionId, error: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerCloseResult {
    pub id: SessionId,
    pub generation: u64,
    pub error: Option<String>,
    pub record: Option<SessionRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerPrunePhase {
    Preview,
    Apply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerPruneModal {
    Confirm {
        preview: PrunePreview,
    },
    Failed {
        phase: PickerPrunePhase,
        preview: Option<PrunePreview>,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerPruneResult {
    Preview {
        generation: u64,
        result: Result<PrunePreview, String>,
    },
    Apply {
        generation: u64,
        preview: PrunePreview,
        removed_ids: Option<Vec<SessionId>>,
        skipped_ids: Option<Vec<SessionId>>,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerPrunePending {
    Preview {
        generation: u64,
    },
    Apply {
        generation: u64,
        preview: PrunePreview,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryView {
    Loading,
    Finished(DiscoveryCompletion),
    RootError,
}

#[derive(Clone, Debug, Default)]
struct CatalogCell {
    sessions: Vec<PickerExternalSession>,
    status: Option<ExternalCatalogStatus>,
    hidden_reserved: usize,
    hidden_unsafe: usize,
    stale: bool,
    loading: bool,
}

impl CatalogCell {
    fn begin_refresh(&mut self) {
        self.stale = !self.sessions.is_empty();
        self.loading = true;
        if self.stale {
            for session in &mut self.sessions {
                if !session.label.starts_with("[stale] ") {
                    session.label = format!("[stale] {}", session.label);
                }
            }
        }
    }

    fn apply(
        &mut self,
        status: ExternalCatalogStatus,
        sessions: Vec<ExternalSession>,
        hidden_reserved: usize,
        hidden_unsafe: usize,
    ) {
        if status == ExternalCatalogStatus::Available || !sessions.is_empty() {
            self.sessions = sessions
                .into_iter()
                .map(|session| {
                    let attached = if session.attached == 0 {
                        "running".to_owned()
                    } else {
                        format!("running · {} attached", session.attached)
                    };
                    PickerExternalSession {
                        label: format!("[external · {attached}] {}", session.name),
                        name: session.name,
                    }
                })
                .collect();
            self.stale = false;
        } else {
            self.stale = !self.sessions.is_empty();
        }
        self.status = Some(status);
        self.hidden_reserved = hidden_reserved;
        self.hidden_unsafe = hidden_unsafe;
        self.loading = false;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResourceIdentity {
    Owned(SessionId),
    External(ExternalSessionName),
    Create,
}
#[derive(Clone, Debug)]
struct StatusCell<T> {
    value: Option<T>,
    stale: bool,
    loading: bool,
}

impl<T> Default for StatusCell<T> {
    fn default() -> Self {
        Self {
            value: None,
            stale: false,
            loading: false,
        }
    }
}

impl<T> StatusCell<T> {
    fn begin_refresh(&mut self) {
        self.stale = self.value.is_some();
        self.loading = self.value.is_none();
    }

    fn apply(&mut self, value: T, _checked_at: std::time::SystemTime) {
        self.value = Some(value);
        self.stale = false;
        self.loading = false;
    }
}

fn format_status_label<T>(
    base: &str,
    cell: Option<&StatusCell<T>>,
    text: impl Fn(&T) -> String,
) -> String {
    match cell {
        Some(cell) if cell.loading => format!("[loading] {base}"),
        Some(cell) if cell.stale => cell.value.as_ref().map_or_else(
            || format!("[loading] {base}"),
            |value| format!("[stale: {}] {base}", text(value)),
        ),
        Some(cell) => cell.value.as_ref().map_or_else(
            || base.to_owned(),
            |value| format!("[{}] {base}", text(value)),
        ),
        None => base.to_owned(),
    }
}

fn host_status_text(status: &HostReachability) -> String {
    match status {
        HostReachability::Reachable => "online",
        HostReachability::Unreachable => "offline",
        HostReachability::TimedOut => "timeout",
        HostReachability::Error => "error",
    }
    .to_owned()
}

fn workload_status_text(status: &WorkloadStatus) -> String {
    match status {
        WorkloadStatus::Running { attached: 0 } => "running".to_owned(),
        WorkloadStatus::Running { attached } => format!("running · {attached} attached"),
        WorkloadStatus::Missing => "missing".to_owned(),
        WorkloadStatus::Unknown => "unknown".to_owned(),
        WorkloadStatus::TimedOut => "timeout".to_owned(),
        WorkloadStatus::Error => "error".to_owned(),
    }
}

#[derive(Clone, Debug)]
pub struct PickerState {
    options: PickerOptions,
    stage: PickerStage,
    host_index: usize,
    resource_index: usize,
    directory_index: usize,
    command_index: usize,
    placement_index: usize,
    resume_id: Option<SessionId>,
    restart_id: Option<SessionId>,
    external_name: Option<ExternalSessionName>,
    generation: u64,
    host_status: HashMap<String, StatusCell<HostReachability>>,
    host_error_details: HashMap<String, String>,
    workload_status: HashMap<SessionId, StatusCell<WorkloadStatus>>,
    catalogs: HashMap<String, CatalogCell>,
    discovery_generation: u64,
    base_directories: HashMap<String, Vec<String>>,
    discovery: HashMap<String, DiscoveryView>,
    directory_filter: String,
    input: PickerInput,
    selected_directory: Option<String>,
    close_modal: Option<PickerCloseModal>,
    close_modal_action: Option<PickerCloseAction>,
    pending_close: Option<(SessionId, u64)>,
    close_failed: HashMap<SessionId, String>,
    retention_days: u64,
    prune_modal: Option<PickerPruneModal>,
    pending_prune: Option<PickerPrunePending>,
    prune_notice: Option<String>,
    operation_error: Option<PickerOperationError>,
}

impl PickerState {
    pub fn new(options: PickerOptions) -> Result<Self> {
        Self::with_retention(options, 30)
    }

    pub fn with_retention(options: PickerOptions, retention_days: u64) -> Result<Self> {
        if options.hosts.is_empty() {
            bail!("the picker has no hosts");
        }
        for host in &options.hosts {
            if host.directories.is_empty() {
                bail!("picker host `{}` has no directories", host.name);
            }
            if host.commands.is_empty() {
                bail!("picker host `{}` has no commands", host.name);
            }
        }

        let base_directories = options
            .hosts
            .iter()
            .filter(|host| host.origin == PickerHostOrigin::Effective)
            .map(|host| (host.name.clone(), host.directories.clone()))
            .collect();
        let placement_index = placement_index(options.default_placement);
        Ok(Self {
            options,
            stage: PickerStage::Host,
            host_index: 0,
            resource_index: 0,
            directory_index: 0,
            command_index: 0,
            placement_index,
            resume_id: None,
            restart_id: None,
            external_name: None,
            generation: 0,
            host_status: HashMap::new(),
            host_error_details: HashMap::new(),
            workload_status: HashMap::new(),
            catalogs: HashMap::new(),
            discovery_generation: 0,
            base_directories,
            discovery: HashMap::new(),
            directory_filter: String::new(),
            input: PickerInput::None,
            selected_directory: None,
            close_modal: None,
            close_modal_action: None,
            pending_close: None,
            close_failed: HashMap::new(),
            retention_days,
            prune_modal: None,
            pending_prune: None,
            prune_notice: None,
            operation_error: None,
        })
    }

    pub fn stage(&self) -> PickerStage {
        self.stage
    }

    pub fn input(&self) -> &PickerInput {
        &self.input
    }

    pub fn operation_error(&self) -> Option<&PickerOperationError> {
        self.operation_error.as_ref()
    }

    pub fn set_operation_error(&mut self, attempted: PickerSelection, diagnostic: impl AsRef<str>) {
        let diagnostic = bounded_error_text(&sanitize_terminal_text(diagnostic.as_ref()));
        self.operation_error = Some(PickerOperationError {
            attempted,
            diagnostic: if diagnostic.is_empty() {
                "Operation failed".to_owned()
            } else {
                diagnostic
            },
        });
    }

    pub fn close_modal(&self) -> Option<&PickerCloseModal> {
        self.close_modal.as_ref()
    }

    pub fn close_busy(&self) -> bool {
        self.pending_close.is_some()
    }

    pub fn prune_modal(&self) -> Option<&PickerPruneModal> {
        self.prune_modal.as_ref()
    }

    pub fn prune_busy(&self) -> bool {
        self.pending_prune.is_some()
    }

    pub fn prune_phase(&self) -> Option<PickerPrunePhase> {
        match (&self.pending_prune, &self.prune_modal) {
            (Some(PickerPrunePending::Preview { .. }), _)
            | (
                None,
                Some(PickerPruneModal::Failed {
                    phase: PickerPrunePhase::Preview,
                    ..
                }),
            ) => Some(PickerPrunePhase::Preview),
            (Some(PickerPrunePending::Apply { .. }), _)
            | (
                None,
                Some(PickerPruneModal::Failed {
                    phase: PickerPrunePhase::Apply,
                    ..
                }),
            ) => Some(PickerPrunePhase::Apply),
            _ => None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn apply_prune_result(&mut self, result: PickerPruneResult) -> bool {
        match result {
            PickerPruneResult::Preview { generation, result } => {
                if self.pending_prune != Some(PickerPrunePending::Preview { generation }) {
                    return false;
                }
                if result
                    .as_ref()
                    .is_ok_and(|preview| preview.older_than_days() != self.retention_days)
                {
                    self.pending_prune = None;
                    self.prune_modal = Some(PickerPruneModal::Failed {
                        phase: PickerPrunePhase::Preview,
                        preview: None,
                        error: "Prune preview retention changed; retry".to_owned(),
                    });
                    return true;
                }
                self.pending_prune = None;
                match result {
                    Ok(preview) if preview.ids().is_empty() => {
                        self.prune_modal = None;
                        self.prune_notice = Some("No closed metadata eligible".to_owned());
                        true
                    }
                    Ok(preview) => {
                        self.prune_notice = None;
                        self.prune_modal = Some(PickerPruneModal::Confirm { preview });
                        true
                    }
                    Err(error) => {
                        self.prune_modal = Some(PickerPruneModal::Failed {
                            phase: PickerPrunePhase::Preview,
                            preview: None,
                            error: bounded_error_text(&sanitize_terminal_text(&error)),
                        });
                        true
                    }
                }
            }
            PickerPruneResult::Apply {
                generation,
                preview,
                removed_ids,
                skipped_ids,
                error,
            } => {
                if self.pending_prune
                    != Some(PickerPrunePending::Apply {
                        generation,
                        preview: preview.clone(),
                    })
                {
                    return false;
                }
                self.pending_prune = None;
                if let Some(error) = error {
                    self.prune_modal = Some(PickerPruneModal::Failed {
                        phase: PickerPrunePhase::Apply,
                        preview: Some(preview),
                        error: bounded_error_text(&sanitize_terminal_text(&error)),
                    });
                    return true;
                }
                let (Some(removed_ids), Some(skipped_ids)) = (removed_ids, skipped_ids) else {
                    self.prune_modal = Some(PickerPruneModal::Failed {
                        phase: PickerPrunePhase::Apply,
                        preview: Some(preview),
                        error: "Prune returned an incomplete result; retry".to_owned(),
                    });
                    return true;
                };
                let expected = preview.ids().iter().copied().collect::<HashSet<_>>();
                let actual = removed_ids
                    .iter()
                    .chain(&skipped_ids)
                    .copied()
                    .collect::<HashSet<_>>();
                if actual != expected || removed_ids.len() + skipped_ids.len() != expected.len() {
                    self.prune_modal = Some(PickerPruneModal::Failed {
                        phase: PickerPrunePhase::Apply,
                        preview: Some(preview),
                        error: "Prune result did not match the confirmed preview; retry".to_owned(),
                    });
                    return true;
                }
                self.prune_modal = None;
                self.prune_notice = Some(format!(
                    "Removed {} closed metadata · skipped {}",
                    removed_ids.len(),
                    skipped_ids.len()
                ));
                let selected_host = self.selected_host_identity();
                let selected_resource = self.current_resource_identity();
                let removed = removed_ids.iter().copied().collect::<HashSet<_>>();
                for host in &mut self.options.hosts {
                    host.workloads
                        .retain(|workload| !removed.contains(&workload.id));
                }
                self.options.hosts.retain(|host| {
                    host.origin == PickerHostOrigin::Effective || !host.workloads.is_empty()
                });
                for id in &removed_ids {
                    self.workload_status.remove(id);
                    self.close_failed.remove(id);
                }
                self.restore_selection(selected_host, selected_resource);
                true
            }
        }
    }

    pub fn apply_close_result(&mut self, result: PickerCloseResult) -> bool {
        if self.pending_close.as_ref() != Some(&(result.id, result.generation)) {
            return false;
        }
        self.pending_close = None;
        let authoritative_absent = result.record.is_none() && result.error.is_none();
        if let Some(record) = &result.record {
            self.reconcile_owned_record(record);
        } else if authoritative_absent {
            let selected_host = self.selected_host_identity();
            let selected_resource = self.current_resource_identity();
            for host in &mut self.options.hosts {
                host.workloads.retain(|workload| workload.id != result.id);
            }
            self.options.hosts.retain(|host| {
                host.origin == PickerHostOrigin::Effective || !host.workloads.is_empty()
            });
            self.workload_status.remove(&result.id);
            self.close_failed.remove(&result.id);
            self.restore_selection(selected_host, selected_resource);
        }
        if let Some(error) = result.error {
            let error = bounded_error_text(&sanitize_terminal_text(&error));
            self.close_failed.insert(result.id, error.clone());
            self.close_modal = Some(PickerCloseModal::Failed {
                id: result.id,
                error,
            });
        } else {
            self.close_failed.remove(&result.id);
            self.close_modal = None;
            self.close_modal_action = None;
        }
        self.rebuild_status_labels();
        true
    }

    fn selected_host_identity(&self) -> Option<(PickerHostOrigin, String, String)> {
        self.options.hosts.get(self.host_index).map(|host| {
            (
                host.origin,
                host.name.clone(),
                host.target.clone().unwrap_or_else(|| "local".to_owned()),
            )
        })
    }

    fn restore_host_identity(&mut self, identity: Option<(PickerHostOrigin, String, String)>) {
        if let Some((origin, name, target)) = identity
            && let Some(index) = self.options.hosts.iter().position(|host| {
                host.origin == origin
                    && host.name == name
                    && host.target.as_deref().unwrap_or("local") == target
            })
        {
            self.host_index = index;
            return;
        }
        self.host_index = self
            .host_index
            .min(self.options.hosts.len().saturating_sub(1));
    }

    fn restore_selection(
        &mut self,
        host_identity: Option<(PickerHostOrigin, String, String)>,
        resource_identity: Option<ResourceIdentity>,
    ) {
        if let Some(ResourceIdentity::Owned(selected_id)) = &resource_identity
            && let Some((host_index, resource_index)) = self
                .options
                .hosts
                .iter()
                .enumerate()
                .find_map(|(host_index, host)| {
                    host.workloads
                        .iter()
                        .position(|workload| workload.id == *selected_id)
                        .map(|resource_index| (host_index, resource_index))
                })
        {
            self.host_index = host_index;
            self.resource_index = resource_index;
            return;
        }
        self.restore_host_identity(host_identity);
        if self.options.hosts.is_empty() {
            self.resource_index = 0;
            self.stage = PickerStage::Host;
            self.resume_id = None;
            self.external_name = None;
            self.selected_directory = None;
            return;
        }
        match resource_identity {
            Some(identity @ (ResourceIdentity::External(_) | ResourceIdentity::Create)) => {
                self.restore_resource_identity(&identity);
            }
            _ => {
                self.resource_index = self
                    .resource_index
                    .min(self.resource_len().saturating_sub(1));
            }
        }
    }

    fn reconcile_owned_record(&mut self, record: &SessionRecord) {
        let selected_host = self.selected_host_identity();
        let selected_resource = self.current_resource_identity();
        for host in &mut self.options.hosts {
            host.workloads.retain(|workload| workload.id != record.id);
        }
        self.options.hosts.retain(|host| {
            host.origin == PickerHostOrigin::Effective || !host.workloads.is_empty()
        });

        if is_normal_session(record) {
            if !self.options.hosts.iter().any(|host| {
                host.name == record.host
                    && host.target.as_deref().unwrap_or("local") == record.target
            }) {
                self.options.hosts.push(PickerHost {
                    name: record.host.clone(),
                    label: format!("{} · retained · {}", record.host, record.target),
                    target: (record.target != "local").then(|| record.target.clone()),
                    origin: PickerHostOrigin::Retained,
                    directories: vec![record.directory.clone()],
                    scan_roots: Vec::new(),
                    commands: vec![PickerCommand::Shell],
                    workloads: Vec::new(),
                    allow_existing: false,
                    allow_create: false,
                });
                let first_retained = self
                    .options
                    .hosts
                    .iter()
                    .position(|host| host.origin == PickerHostOrigin::Retained)
                    .unwrap_or(self.options.hosts.len());
                self.options.hosts[first_retained..].sort_by(|left, right| {
                    (&left.name, left.target.as_deref().unwrap_or("local"))
                        .cmp(&(&right.name, right.target.as_deref().unwrap_or("local")))
                });
            }
            let host_index = self
                .options
                .hosts
                .iter()
                .position(|host| {
                    host.name == record.host
                        && host.target.as_deref().unwrap_or("local") == record.target
                })
                .expect("reconciled host group exists");
            let label = workload_label(record);
            self.options.hosts[host_index]
                .workloads
                .push(PickerWorkload {
                    id: record.id,
                    status: record.status,
                    legacy: record.ownership_proof.is_none(),
                    last_used_at: record.last_used_at,
                    base_label: label.clone(),
                    label,
                });
            self.options.hosts[host_index]
                .workloads
                .sort_by(|left, right| {
                    compare_normal_sessions(
                        left.status,
                        left.last_used_at,
                        left.id,
                        right.status,
                        right.last_used_at,
                        right.id,
                    )
                });
        }
        self.restore_selection(selected_host, selected_resource);
    }

    pub fn directory_paths(&self, host: &str) -> Option<Vec<&str>> {
        self.options
            .hosts
            .iter()
            .find(|candidate| candidate.name == host)
            .map(|candidate| candidate.directories.iter().map(String::as_str).collect())
    }

    pub fn visible_directories(&self) -> Vec<&str> {
        self.visible_directory_indices()
            .into_iter()
            .map(|index| self.options.hosts[self.host_index].directories[index].as_str())
            .collect()
    }

    pub fn begin_discovery(&mut self, generation: u64) {
        self.discovery_generation = generation;
        self.discovery.clear();
        for host in self
            .options
            .hosts
            .iter_mut()
            .filter(|host| host.origin == PickerHostOrigin::Effective)
        {
            if let Some(base) = self.base_directories.get(&host.name) {
                host.directories.clone_from(base);
            }
            self.discovery
                .insert(host.name.clone(), DiscoveryView::Loading);
        }
        self.directory_index = 0;
    }

    pub fn apply_discovery(&mut self, message: DiscoveryMessage) -> bool {
        if message.generation() != self.discovery_generation {
            return false;
        }
        match message {
            DiscoveryMessage::Repository { host, path, .. } => {
                let Some(host) = self.options.hosts.iter_mut().find(|candidate| {
                    candidate.origin == PickerHostOrigin::Effective && candidate.name == host
                }) else {
                    return false;
                };
                push_unique(&mut host.directories, &path);
                self.clamp_directory_index();
                true
            }
            DiscoveryMessage::RootError { host, .. } => {
                let Some(view) = self.discovery.get_mut(&host) else {
                    return false;
                };
                *view = DiscoveryView::RootError;
                true
            }
            DiscoveryMessage::HostFinished {
                host, completion, ..
            } => {
                let Some(view) = self.discovery.get_mut(&host) else {
                    return false;
                };
                if *view != DiscoveryView::RootError || completion != DiscoveryCompletion::Complete
                {
                    *view = DiscoveryView::Finished(completion);
                }
                true
            }
            DiscoveryMessage::Finished { .. } => false,
        }
    }

    fn discovery_request(&self) -> DiscoveryRequest {
        DiscoveryRequest {
            generation: self.discovery_generation,
            locations: self
                .options
                .hosts
                .iter()
                .filter(|host| host.origin == PickerHostOrigin::Effective)
                .map(|host| DiscoveryLocation {
                    host: host.name.clone(),
                    target: host.target.clone(),
                    roots: host.scan_roots.clone(),
                })
                .collect(),
        }
    }

    fn visible_directory_indices(&self) -> Vec<usize> {
        let query = self.directory_filter.to_lowercase();
        self.options.hosts[self.host_index]
            .directories
            .iter()
            .enumerate()
            .filter_map(|(index, directory)| {
                (query.is_empty() || directory.to_lowercase().contains(&query)).then_some(index)
            })
            .collect()
    }

    fn clamp_directory_index(&mut self) {
        let length = self.visible_directory_indices().len();
        self.directory_index = self.directory_index.min(length.saturating_sub(1));
    }

    pub fn host_label(&self, name: &str) -> Option<&str> {
        self.options
            .hosts
            .iter()
            .find(|host| host.name == name)
            .map(|host| host.label.as_str())
    }

    pub fn workload_label(&self, id: SessionId) -> Option<&str> {
        self.options
            .hosts
            .iter()
            .flat_map(|host| &host.workloads)
            .find(|workload| workload.id == id)
            .map(|workload| workload.label.as_str())
    }

    pub fn resource_labels(&self, host: &str) -> Option<Vec<String>> {
        let host = self
            .options
            .hosts
            .iter()
            .find(|candidate| candidate.name == host)?;
        let mut labels = host
            .workloads
            .iter()
            .map(|workload| workload.label.clone())
            .collect::<Vec<_>>();
        if host.allow_existing
            && let Some(catalog) = self.catalogs.get(&host.name)
        {
            labels.extend(catalog.sessions.iter().map(|session| session.label.clone()));
        }
        if host.allow_create {
            labels.push("Create new Tether workload".to_owned());
        }
        Some(labels)
    }

    pub fn begin_refresh(&mut self, generation: u64) {
        self.generation = generation;
        for host in &self.options.hosts {
            self.host_status
                .entry(host.name.clone())
                .or_default()
                .begin_refresh();
            for workload in host
                .workloads
                .iter()
                .filter(|workload| workload.status == SessionStatus::Running && !workload.legacy)
            {
                self.workload_status
                    .entry(workload.id)
                    .or_default()
                    .begin_refresh();
            }
            if host.allow_existing {
                self.catalogs
                    .entry(host.name.clone())
                    .or_default()
                    .begin_refresh();
            }
        }
        self.rebuild_status_labels();
    }

    pub fn apply_status(&mut self, message: StatusMessage) -> bool {
        if message.generation() != self.generation {
            return false;
        }
        let selected_resource = self.current_resource_identity();
        let applied = match message {
            StatusMessage::Host {
                host,
                status,
                detail,
                checked_at,
                ..
            } => {
                let applied = self.host_status.get_mut(&host).is_some_and(|cell| {
                    cell.apply(status, checked_at);
                    true
                });
                if applied {
                    if let Some(detail) = detail {
                        self.host_error_details
                            .insert(host, bounded_error_text(&sanitize_terminal_text(&detail)));
                    } else {
                        self.host_error_details.remove(&host);
                    }
                }
                applied
            }
            StatusMessage::Workload {
                id,
                status,
                checked_at,
                ..
            } => {
                if status == WorkloadStatus::Missing {
                    self.mark_workload_ended(id);
                }
                self.workload_status.get_mut(&id).is_some_and(|cell| {
                    cell.apply(status, checked_at);
                    true
                })
            }
            StatusMessage::Catalog {
                host,
                status,
                sessions,
                hidden_reserved,
                hidden_unsafe,
                ..
            } => self.catalogs.get_mut(&host).is_some_and(|cell| {
                cell.apply(status, sessions, hidden_reserved, hidden_unsafe);
                true
            }),
            StatusMessage::Finished { .. } => false,
        };
        if applied {
            self.rebuild_status_labels();
            if let Some(identity) = selected_resource {
                self.restore_resource_identity(&identity);
            }
        }
        applied
    }

    fn current_resource_identity(&self) -> Option<ResourceIdentity> {
        let host = &self.options.hosts[self.host_index];
        if let Some(workload) = host.workloads.get(self.resource_index) {
            return Some(ResourceIdentity::Owned(workload.id));
        }
        let external_index = self.resource_index.saturating_sub(host.workloads.len());
        if host.allow_existing
            && let Some(session) = self
                .catalogs
                .get(&host.name)
                .and_then(|catalog| catalog.sessions.get(external_index))
        {
            return Some(ResourceIdentity::External(session.name.clone()));
        }
        host.allow_create.then_some(ResourceIdentity::Create)
    }

    fn restore_resource_identity(&mut self, identity: &ResourceIdentity) {
        let host = &self.options.hosts[self.host_index];
        self.resource_index = match identity {
            ResourceIdentity::Owned(id) => host
                .workloads
                .iter()
                .position(|workload| workload.id == *id)
                .unwrap_or(0),
            ResourceIdentity::External(name) => self
                .catalogs
                .get(&host.name)
                .and_then(|catalog| {
                    catalog
                        .sessions
                        .iter()
                        .position(|session| &session.name == name)
                })
                .map(|index| host.workloads.len() + index)
                .unwrap_or_else(|| self.resource_len().saturating_sub(1)),
            ResourceIdentity::Create => self.resource_len().saturating_sub(1),
        };
    }

    fn resource_len(&self) -> usize {
        let host = &self.options.hosts[self.host_index];
        host.workloads.len()
            + if host.allow_existing {
                self.catalogs
                    .get(&host.name)
                    .map_or(0, |catalog| catalog.sessions.len())
            } else {
                0
            }
            + usize::from(host.allow_create)
    }

    fn status_request(&self) -> StatusRequest {
        StatusRequest {
            generation: self.generation,
            hosts: self
                .options
                .hosts
                .iter()
                .map(|host| StatusHost {
                    name: host.name.clone(),
                    target: host.target.clone(),
                    workloads: host
                        .workloads
                        .iter()
                        .filter(|workload| {
                            workload.status == SessionStatus::Running && !workload.legacy
                        })
                        .map(|workload| workload.id)
                        .collect(),
                })
                .collect(),
        }
    }

    fn mark_workload_ended(&mut self, id: SessionId) {
        for workload in self
            .options
            .hosts
            .iter_mut()
            .flat_map(|host| &mut host.workloads)
            .filter(|workload| workload.id == id)
        {
            workload.status = SessionStatus::Ended;
            workload.base_label = workload
                .base_label
                .replacen("[running]", "[ended]", 1)
                .replacen(" · Open ", " · Restart ", 1);
        }
    }

    fn current_host_reachable(&self) -> bool {
        let Some(host) = self.options.hosts.get(self.host_index) else {
            return false;
        };
        self.host_status
            .get(&host.name)
            .is_some_and(|cell| !cell.stale && cell.value == Some(HostReachability::Reachable))
    }
    fn current_host_unreachable(&self) -> bool {
        let Some(host) = self.options.hosts.get(self.host_index) else {
            return false;
        };
        self.host_status.get(&host.name).is_some_and(|cell| {
            !cell.stale
                && cell
                    .value
                    .is_some_and(|status| status != HostReachability::Reachable)
        })
    }

    fn current_legacy_id(&self) -> Option<SessionId> {
        if self.stage != PickerStage::Resource {
            return None;
        }
        let ResourceIdentity::Owned(id) = self.current_resource_identity()? else {
            return None;
        };
        self.options.hosts[self.host_index]
            .workloads
            .iter()
            .find(|workload| {
                workload.id == id && workload.legacy && workload.status != SessionStatus::Removed
            })
            .map(|workload| workload.id)
    }
    fn current_owned_action(&self) -> Option<(SessionId, bool)> {
        if self.stage != PickerStage::Resource || !self.current_host_reachable() {
            return None;
        }
        let ResourceIdentity::Owned(id) = self.current_resource_identity()? else {
            return None;
        };
        let workload = self.options.hosts[self.host_index]
            .workloads
            .iter()
            .find(|workload| workload.id == id)?;
        if workload.legacy {
            return None;
        }
        match workload.status {
            SessionStatus::Ended => Some((id, true)),
            SessionStatus::Running
                if self.workload_status.get(&id).is_some_and(|cell| {
                    !cell.stale && matches!(cell.value, Some(WorkloadStatus::Running { .. }))
                }) =>
            {
                Some((id, false))
            }
            _ => None,
        }
    }

    fn rebuild_status_labels(&mut self) {
        for host in &mut self.options.hosts {
            if host.origin == PickerHostOrigin::Effective {
                host.label = format_status_label(
                    &host.name,
                    self.host_status.get(&host.name),
                    host_status_text,
                );
                if let Some(detail) = self.host_error_details.get(&host.name) {
                    host.label.push_str(" · ");
                    host.label.push_str(detail);
                }
            }
            for workload in &mut host.workloads {
                workload.label = if workload.status == SessionStatus::Running {
                    format_status_label(
                        &workload.base_label,
                        self.workload_status.get(&workload.id),
                        workload_status_text,
                    )
                } else {
                    workload.base_label.clone()
                };
                if self
                    .pending_close
                    .as_ref()
                    .is_some_and(|(id, _)| *id == workload.id)
                {
                    workload.label = format!("[applying…] {}", workload.base_label);
                } else if self.close_failed.contains_key(&workload.id) {
                    workload.label = format!("[action failed · x retry] {}", workload.base_label);
                }
            }
        }
    }

    pub fn handle(&mut self, event: PickerEvent) -> PickerOutcome {
        if self.operation_error.is_some() {
            return match event {
                PickerEvent::Confirm => {
                    let error = self
                        .operation_error
                        .take()
                        .expect("operation error was checked");
                    PickerOutcome::Selected(error.attempted)
                }
                PickerEvent::Back | PickerEvent::Cancel => {
                    self.operation_error = None;
                    PickerOutcome::Continue
                }
                _ => PickerOutcome::Continue,
            };
        }
        if self.pending_close.is_some()
            && !matches!(event, PickerEvent::Previous | PickerEvent::Next)
        {
            return PickerOutcome::Continue;
        }
        if let Some(pending) = &self.pending_prune {
            if matches!(pending, PickerPrunePending::Preview { .. })
                && matches!(event, PickerEvent::Cancel)
            {
                return PickerOutcome::Cancelled;
            }
            if !matches!(event, PickerEvent::Previous | PickerEvent::Next) {
                return PickerOutcome::Continue;
            }
        }
        if matches!(event, PickerEvent::Cancel) {
            return PickerOutcome::Cancelled;
        }
        if self.close_modal.is_some() {
            return match event {
                PickerEvent::DismissClose => {
                    self.close_modal = None;
                    self.close_modal_action = None;
                    PickerOutcome::Continue
                }
                PickerEvent::ConfirmClose => self.confirm_close(),
                _ => PickerOutcome::Continue,
            };
        }
        if self.prune_modal.is_some() {
            return match event {
                PickerEvent::DismissPrune => {
                    self.prune_modal = None;
                    PickerOutcome::Continue
                }
                PickerEvent::ConfirmPrune => self.confirm_prune(),
                _ => PickerOutcome::Continue,
            };
        }
        if self.input != PickerInput::None {
            return self.handle_input(event);
        }
        match event {
            PickerEvent::ManageObservers => {
                PickerOutcome::Selected(PickerSelection::ManageObservers)
            }
            PickerEvent::Refresh => PickerOutcome::RefreshRequested,
            PickerEvent::Close => self.begin_close(),
            PickerEvent::BeginPrune => self.begin_prune(),
            PickerEvent::BeginFilter if self.stage == PickerStage::Directory => {
                self.input = PickerInput::Filter(self.directory_filter.clone());
                PickerOutcome::Continue
            }
            PickerEvent::BeginPath if self.stage == PickerStage::Directory => {
                self.input = PickerInput::DirectPath(String::new());
                PickerOutcome::Continue
            }
            PickerEvent::Back => self.back(),
            PickerEvent::Previous => {
                let length = self.current_len();
                if length == 0 {
                    return PickerOutcome::Continue;
                }
                let index = self.current_index_mut();
                *index = if *index == 0 { length - 1 } else { *index - 1 };
                PickerOutcome::Continue
            }
            PickerEvent::Next => {
                let length = self.current_len();
                if length == 0 {
                    return PickerOutcome::Continue;
                }
                let index = self.current_index_mut();
                *index = (*index + 1) % length;
                PickerOutcome::Continue
            }
            PickerEvent::Confirm => self.confirm(),
            PickerEvent::Cancel
            | PickerEvent::ConfirmClose
            | PickerEvent::DismissClose
            | PickerEvent::ConfirmPrune
            | PickerEvent::DismissPrune
            | PickerEvent::BeginFilter
            | PickerEvent::BeginPath
            | PickerEvent::Insert(_)
            | PickerEvent::Delete
            | PickerEvent::SubmitInput
            | PickerEvent::ExitInput => PickerOutcome::Continue,
        }
    }

    fn begin_prune(&mut self) -> PickerOutcome {
        self.prune_notice = None;
        self.pending_prune = Some(PickerPrunePending::Preview {
            generation: self.generation,
        });
        PickerOutcome::PrunePreviewRequested {
            older_than_days: self.retention_days,
            generation: self.generation,
        }
    }

    fn confirm_prune(&mut self) -> PickerOutcome {
        let modal = self.prune_modal.take();
        match modal {
            Some(PickerPruneModal::Confirm { preview })
            | Some(PickerPruneModal::Failed {
                phase: PickerPrunePhase::Apply,
                preview: Some(preview),
                ..
            }) => {
                let generation = self.generation;
                self.pending_prune = Some(PickerPrunePending::Apply {
                    generation,
                    preview: preview.clone(),
                });
                PickerOutcome::PruneApplyRequested {
                    preview,
                    generation,
                }
            }
            Some(PickerPruneModal::Failed {
                phase: PickerPrunePhase::Preview,
                ..
            }) => self.begin_prune(),
            other => {
                self.prune_modal = other;
                PickerOutcome::Continue
            }
        }
    }
    fn begin_close(&mut self) -> PickerOutcome {
        if self.pending_close.is_some() {
            return PickerOutcome::Continue;
        }
        if let Some(id) = self.current_legacy_id() {
            self.close_modal_action = Some(PickerCloseAction::Remove);
            self.close_modal = Some(PickerCloseModal::Confirm { id });
            return PickerOutcome::Continue;
        }
        let Some((id, restart)) = self.current_owned_action() else {
            return PickerOutcome::Continue;
        };
        self.close_modal_action = Some(if restart {
            PickerCloseAction::Remove
        } else {
            PickerCloseAction::Stop
        });
        self.close_modal = Some(PickerCloseModal::Confirm { id });
        PickerOutcome::Continue
    }

    fn confirm_close(&mut self) -> PickerOutcome {
        if self.pending_close.is_some() {
            return PickerOutcome::Continue;
        }
        let Some(id) = self.close_modal.as_ref().map(|modal| match modal {
            PickerCloseModal::Confirm { id } | PickerCloseModal::Failed { id, .. } => *id,
        }) else {
            return PickerOutcome::Continue;
        };
        let Some(action) = self.close_modal_action else {
            self.close_modal = None;
            return PickerOutcome::Continue;
        };
        self.close_modal = None;
        self.pending_close = Some((id, self.generation));
        self.rebuild_status_labels();
        PickerOutcome::CloseOwnedRequested {
            id,
            generation: self.generation,
            action,
        }
    }

    fn handle_input(&mut self, event: PickerEvent) -> PickerOutcome {
        match event {
            PickerEvent::Cancel => PickerOutcome::Cancelled,
            PickerEvent::ExitInput => {
                self.input = PickerInput::None;
                PickerOutcome::Continue
            }
            PickerEvent::Insert(character) => {
                if let PickerInput::Filter(query) | PickerInput::DirectPath(query) = &mut self.input
                {
                    query.push(character);
                    if let PickerInput::Filter(query) = &self.input {
                        self.directory_filter.clone_from(query);
                        self.directory_index = 0;
                    }
                }
                PickerOutcome::Continue
            }
            PickerEvent::Delete => {
                if let PickerInput::Filter(query) | PickerInput::DirectPath(query) = &mut self.input
                {
                    query.pop();
                    if let PickerInput::Filter(query) = &self.input {
                        self.directory_filter.clone_from(query);
                        self.clamp_directory_index();
                    }
                }
                PickerOutcome::Continue
            }
            PickerEvent::SubmitInput => {
                let input = std::mem::replace(&mut self.input, PickerInput::None);
                match input {
                    PickerInput::Filter(query) => {
                        self.directory_filter = query;
                        self.clamp_directory_index();
                    }
                    PickerInput::DirectPath(path) if !path.trim().is_empty() => {
                        self.selected_directory = Some(path);
                        self.directory_filter.clear();
                        self.stage = PickerStage::Command;
                    }
                    PickerInput::DirectPath(path) => {
                        self.input = PickerInput::DirectPath(path);
                    }
                    PickerInput::None => {}
                }
                PickerOutcome::Continue
            }
            _ => PickerOutcome::Continue,
        }
    }

    fn back(&mut self) -> PickerOutcome {
        self.stage = match self.stage {
            PickerStage::Host => return PickerOutcome::Cancelled,
            PickerStage::Resource => PickerStage::Host,
            PickerStage::Directory => PickerStage::Resource,
            PickerStage::Command => PickerStage::Directory,
            PickerStage::Placement
                if self.resume_id.is_some()
                    || self.restart_id.is_some()
                    || self.external_name.is_some() =>
            {
                PickerStage::Resource
            }
            PickerStage::Placement => PickerStage::Command,
        };
        self.resume_id = None;
        self.restart_id = None;
        self.external_name = None;
        PickerOutcome::Continue
    }

    fn confirm(&mut self) -> PickerOutcome {
        match self.stage {
            PickerStage::Host => {
                if self.options.hosts.is_empty() {
                    return PickerOutcome::Continue;
                }
                self.resource_index = 0;
                self.directory_index = 0;
                self.command_index = 0;
                self.resume_id = None;
                self.restart_id = None;
                self.external_name = None;
                self.selected_directory = None;
                self.directory_filter.clear();
                self.stage = PickerStage::Resource;
                PickerOutcome::Continue
            }
            PickerStage::Resource => {
                if self.current_host_unreachable() {
                    return PickerOutcome::Continue;
                }
                let host = &self.options.hosts[self.host_index];
                if let Some(workload) = host.workloads.get(self.resource_index) {
                    let Some((id, restart)) = self.current_owned_action() else {
                        return PickerOutcome::Continue;
                    };
                    if id != workload.id || self.close_failed.contains_key(&id) {
                        return PickerOutcome::Continue;
                    }
                    self.resume_id = (!restart).then_some(id);
                    self.restart_id = restart.then_some(id);
                    self.external_name = None;
                    self.stage = PickerStage::Placement;
                    return PickerOutcome::Continue;
                }
                let external_index = self.resource_index.saturating_sub(host.workloads.len());
                if host.allow_existing
                    && let Some(session) = self
                        .catalogs
                        .get(&host.name)
                        .and_then(|catalog| catalog.sessions.get(external_index))
                {
                    self.resume_id = None;
                    self.restart_id = None;
                    self.external_name = Some(session.name.clone());
                    self.stage = PickerStage::Placement;
                    return PickerOutcome::Continue;
                }
                if host.allow_create {
                    self.resume_id = None;
                    self.restart_id = None;
                    self.external_name = None;
                    self.selected_directory = None;
                    self.directory_filter.clear();
                    self.stage = PickerStage::Directory;
                }
                PickerOutcome::Continue
            }
            PickerStage::Directory => {
                let indices = self.visible_directory_indices();
                let Some(index) = indices.get(self.directory_index) else {
                    return PickerOutcome::Continue;
                };
                self.selected_directory =
                    Some(self.options.hosts[self.host_index].directories[*index].clone());
                self.stage = PickerStage::Command;
                PickerOutcome::Continue
            }
            PickerStage::Command => {
                self.stage = PickerStage::Placement;
                PickerOutcome::Continue
            }
            PickerStage::Placement => PickerOutcome::Selected(self.selection()),
        }
    }

    fn selection(&self) -> PickerSelection {
        let placement = PLACEMENTS[self.placement_index];
        if let Some(id) = self.resume_id {
            return PickerSelection::Resume { id, placement };
        }
        if let Some(id) = self.restart_id {
            return PickerSelection::Restart { id, placement };
        }
        if let Some(name) = &self.external_name {
            let host = &self.options.hosts[self.host_index];
            return PickerSelection::AttachExternal {
                host: host.name.clone(),
                target: host.target.clone(),
                name: name.clone(),
                placement,
            };
        }

        let host = &self.options.hosts[self.host_index];
        let (preset, command) = host.commands[self.command_index].selection_parts();
        PickerSelection::Create(OpenSelection {
            host: host.name.clone(),
            directory: self
                .selected_directory
                .clone()
                .expect("create selection has a directory"),
            preset,
            command,
            placement,
        })
    }

    fn current_len(&self) -> usize {
        match self.stage {
            PickerStage::Host => self.options.hosts.len(),
            PickerStage::Resource => self.resource_len(),
            PickerStage::Directory => self.visible_directory_indices().len(),
            PickerStage::Command => self.options.hosts[self.host_index].commands.len(),
            PickerStage::Placement => PLACEMENTS.len(),
        }
    }

    fn current_index(&self) -> usize {
        match self.stage {
            PickerStage::Host => self.host_index,
            PickerStage::Resource => self.resource_index,
            PickerStage::Directory => self.directory_index,
            PickerStage::Command => self.command_index,
            PickerStage::Placement => self.placement_index,
        }
    }

    fn current_index_mut(&mut self) -> &mut usize {
        match self.stage {
            PickerStage::Host => &mut self.host_index,
            PickerStage::Resource => &mut self.resource_index,
            PickerStage::Directory => &mut self.directory_index,
            PickerStage::Command => &mut self.command_index,
            PickerStage::Placement => &mut self.placement_index,
        }
    }

    fn title(&self) -> &'static str {
        match self.stage {
            PickerStage::Host => "Hosts",
            PickerStage::Resource => "Resources",
            PickerStage::Directory => "Directory",
            PickerStage::Command => "Shell or preset",
            PickerStage::Placement => "Placement",
        }
    }

    fn item_labels(&self) -> Vec<&str> {
        match self.stage {
            PickerStage::Host => self
                .options
                .hosts
                .iter()
                .map(|host| host.label.as_str())
                .collect(),
            PickerStage::Resource => {
                let host = &self.options.hosts[self.host_index];
                let mut labels = host
                    .workloads
                    .iter()
                    .map(|workload| workload.label.as_str())
                    .collect::<Vec<_>>();
                if host.allow_existing
                    && let Some(catalog) = self.catalogs.get(&host.name)
                {
                    labels.extend(
                        catalog
                            .sessions
                            .iter()
                            .map(|session| session.label.as_str()),
                    );
                }
                if host.allow_create {
                    labels.push("Create new Tether workload");
                }
                labels
            }
            PickerStage::Directory => {
                let host = &self.options.hosts[self.host_index];
                let mut labels = self
                    .visible_directory_indices()
                    .into_iter()
                    .map(|index| host.directories[index].as_str())
                    .collect::<Vec<_>>();
                if labels.is_empty() {
                    labels.push("No matches · p enter path");
                }
                labels
            }
            PickerStage::Command => self.options.hosts[self.host_index]
                .commands
                .iter()
                .map(PickerCommand::label)
                .collect(),
            PickerStage::Placement => PLACEMENT_LABELS.to_vec(),
        }
    }
    fn discovery_label(&self, host: &str) -> Option<&'static str> {
        match self.discovery.get(host) {
            Some(DiscoveryView::Loading) => Some("Scanning repositories…"),
            Some(DiscoveryView::RootError) => Some("Repository scan error · r retry"),
            Some(DiscoveryView::Finished(DiscoveryCompletion::Complete)) | None => None,
            Some(DiscoveryView::Finished(DiscoveryCompletion::ResultsLimit)) => {
                Some("Repository result limit reached")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::EntriesLimit)) => {
                Some("Repository entry limit reached")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::TimedOut)) => {
                Some("Repository scan timed out · r retry")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::Unavailable)) => {
                Some("Repository scan unavailable · r retry")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::OutputLimit)) => {
                Some("Repository output limit reached")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::Malformed)) => {
                Some("Repository scan returned invalid data")
            }
            Some(DiscoveryView::Finished(DiscoveryCompletion::Error)) => {
                Some("Repository scan error · r retry")
            }
        }
    }

    fn catalog_label(&self, host: &str) -> Option<String> {
        let catalog = self.catalogs.get(host)?;
        if catalog.loading {
            return Some(if catalog.stale {
                "Refreshing external sessions · stale rows remain attachable".to_owned()
            } else {
                "Discovering external tmux sessions…".to_owned()
            });
        }
        let mut notices = Vec::new();
        match catalog.status {
            Some(ExternalCatalogStatus::Available) if catalog.sessions.is_empty() => {
                notices.push("No attachable external tmux sessions".to_owned());
            }
            Some(ExternalCatalogStatus::Available) => {}
            Some(ExternalCatalogStatus::Unavailable) => {
                notices.push("External sessions unavailable · r retry".to_owned());
            }
            Some(ExternalCatalogStatus::TimedOut) => {
                notices.push("External session probe timed out · r retry".to_owned());
            }
            Some(ExternalCatalogStatus::Error) if catalog.stale => {
                notices.push("External refresh error · stale rows remain attachable".to_owned());
            }
            Some(ExternalCatalogStatus::Error) => {
                notices.push("External session response invalid · r retry".to_owned());
            }
            None => {}
        }
        if catalog.hidden_reserved > 0 {
            notices.push(format!(
                "{} reserved Tether-like session(s) hidden",
                catalog.hidden_reserved
            ));
        }
        if catalog.hidden_unsafe > 0 {
            notices.push(format!(
                "{} unsafe/unrenderable session name(s) hidden",
                catalog.hidden_unsafe
            ));
        }
        (!notices.is_empty()).then(|| notices.join(" · "))
    }

    fn close_action(&self, id: SessionId) -> PickerCloseAction {
        if self
            .options
            .hosts
            .iter()
            .flat_map(|host| &host.workloads)
            .any(|workload| {
                workload.id == id && (workload.legacy || workload.status == SessionStatus::Ended)
            })
        {
            PickerCloseAction::Remove
        } else {
            PickerCloseAction::Stop
        }
    }

    fn modal_close_action(&self, id: SessionId) -> PickerCloseAction {
        self.close_modal_action
            .unwrap_or_else(|| self.close_action(id))
    }

    fn modal_text(&self) -> Option<String> {
        if let Some(error) = &self.operation_error {
            return Some(format!(
                "Enter retry · Backspace/Esc cancel · Operation failed: {}",
                error.diagnostic
            ));
        }
        if let Some(modal) = &self.prune_modal {
            return Some(match modal {
                PickerPruneModal::Confirm { preview } => format!(
                    "{} closed metadata older than {} days · y confirm · n/Esc keep · No host contact · IDs: session prune --dry-run",
                    preview.ids().len(),
                    preview.older_than_days()
                ),
                PickerPruneModal::Failed { phase, error, .. } => format!(
                    "y retry · n/Esc dismiss · {} failed: {error} · No host contact",
                    match phase {
                        PickerPrunePhase::Preview => "Preview",
                        PickerPrunePhase::Apply => "Prune",
                    }
                ),
            });
        }
        match self.close_modal.as_ref()? {
            PickerCloseModal::Confirm { id } => match self.modal_close_action(*id) {
                PickerCloseAction::Stop => Some(format!(
                    "y confirm · n/Esc keep · Stop exact {id}? Ends its Tether workload."
                )),
                PickerCloseAction::Remove if self.current_legacy_id() == Some(*id) => {
                    Some(format!(
                        "y confirm · n/Esc keep · Remove legacy record {id}? Metadata only; any same-named tmux session is untouched."
                    ))
                }
                PickerCloseAction::Remove => Some(format!(
                    "y confirm · n/Esc keep · Remove ended {id}? Metadata only; no live workload is touched."
                )),
            },
            PickerCloseModal::Failed { id, error } => Some(format!(
                "y retry · n/Esc dismiss · {} failed: {error}",
                match self.modal_close_action(*id) {
                    PickerCloseAction::Stop => "Stop",
                    PickerCloseAction::Remove => "Remove",
                }
            )),
        }
    }

    pub fn frame_title(&self) -> String {
        if self.operation_error.is_some() {
            return "Operation failed".to_owned();
        }
        match (&self.prune_modal, &self.pending_prune, &self.close_modal) {
            (Some(PickerPruneModal::Confirm { .. }), _, _) => "Confirm prune".to_owned(),
            (Some(PickerPruneModal::Failed { .. }), _, _) => "Prune failed".to_owned(),
            (_, Some(PickerPrunePending::Preview { .. }), _) => "Previewing prune".to_owned(),
            (_, Some(PickerPrunePending::Apply { .. }), _) => "Pruning metadata".to_owned(),
            (_, _, Some(PickerCloseModal::Confirm { id })) => match self.modal_close_action(*id) {
                PickerCloseAction::Stop => "Confirm Stop".to_owned(),
                PickerCloseAction::Remove => "Confirm Remove".to_owned(),
            },
            (_, _, Some(PickerCloseModal::Failed { id, .. })) => match self.modal_close_action(*id)
            {
                PickerCloseAction::Stop => "Stop failed".to_owned(),
                PickerCloseAction::Remove => "Remove failed".to_owned(),
            },
            (_, _, None) if self.pending_close.is_some() => "Applying lifecycle action".to_owned(),
            _ => self.title().to_owned(),
        }
    }

    pub fn footer_text(&self) -> String {
        if let Some((id, _)) = self.pending_close {
            return format!("Applying confirmed action · wait for result · {id} · ↑/↓ navigate");
        }
        if let Some(pending) = &self.pending_prune {
            return match pending {
                PickerPrunePending::Preview { .. } => {
                    "Previewing closed metadata · wait for result · No host contact · ↑/↓ navigate"
                        .to_owned()
                }
                PickerPrunePending::Apply { preview, .. } => format!(
                    "Pruning {} closed metadata · wait for result · No host contact · ↑/↓ navigate",
                    preview.ids().len()
                ),
            };
        }
        if let Some(text) = self.modal_text() {
            return text;
        }
        match &self.input {
            PickerInput::Filter(query) => {
                format!("Filter: {query} · type · Backspace delete · Enter/Esc close")
            }
            PickerInput::DirectPath(path) => {
                format!("Path: {path} · type · Backspace delete · Enter use · Esc close")
            }
            PickerInput::None => {
                let mut parts = Vec::new();
                if let Some(notice) = &self.prune_notice {
                    parts.push(notice.clone());
                }
                if !self.directory_filter.is_empty() && self.stage == PickerStage::Directory {
                    parts.push(format!("Filter: {}", self.directory_filter));
                }
                if self.stage == PickerStage::Directory
                    && let Some(label) =
                        self.discovery_label(&self.options.hosts[self.host_index].name)
                {
                    parts.push(label.to_owned());
                }
                if self.stage == PickerStage::Resource
                    && self.options.hosts[self.host_index].allow_existing
                    && let Some(label) =
                        self.catalog_label(&self.options.hosts[self.host_index].name)
                {
                    parts.push(label);
                }
                let (primary_hint, destructive_hint) = if self.stage == PickerStage::Resource {
                    if self.current_legacy_id().is_some() {
                        ("", " · x Remove")
                    } else {
                        match self.current_owned_action() {
                            Some((_, false)) => ("Enter Open", " · x Stop"),
                            Some((_, true)) => ("Enter Restart", " · x Remove"),
                            None => match self.current_resource_identity() {
                                Some(ResourceIdentity::External(_))
                                | Some(ResourceIdentity::Create)
                                    if !self.current_host_unreachable() =>
                                {
                                    ("Enter select", "")
                                }
                                _ => ("", ""),
                            },
                        }
                    }
                } else if self.stage == PickerStage::Host && self.options.hosts.is_empty() {
                    ("", "")
                } else {
                    ("Enter select", "")
                };
                let primary_hint = if primary_hint.is_empty() {
                    String::new()
                } else {
                    format!("\n{primary_hint} · ")
                };
                let path_hint = if self.stage == PickerStage::Directory {
                    " · / filter · p path"
                } else {
                    ""
                };
                let refresh_hint = if self.current_host_unreachable() {
                    "Retry"
                } else {
                    "Refresh"
                };
                let back_action = if self.stage == PickerStage::Host {
                    "close"
                } else {
                    "back"
                };
                parts.insert(
                    0,
                    format!(
                        "Esc {back_action} · {primary_hint}↑/↓ navigate{destructive_hint}{path_hint} · o Observers · r {refresh_hint} · Backspace {back_action}"
                    ),
                );
                parts.join(" · ")
            }
        }
    }
}

fn terminal_safe_text(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_control) {
        Cow::Owned(sanitize_terminal_text(text))
    } else {
        Cow::Borrowed(text)
    }
}

fn sanitize_terminal_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
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
                        if sequence_character == '\u{7}'
                            || (sequence_character == '\u{1b}'
                                && characters.next_if_eq(&'\\').is_some())
                        {
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
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bounded_error_text(text: &str) -> String {
    const MAX_ERROR_CHARACTERS: usize = 240;
    let mut characters = text.chars();
    let bounded = characters
        .by_ref()
        .take(MAX_ERROR_CHARACTERS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

pub fn format_close_error(error: CloseOwnedError) -> String {
    sanitize_terminal_text(&format!("{:#}", anyhow::Error::new(error)))
}

fn format_prune_error(error: crate::lifecycle::PruneError) -> String {
    sanitize_terminal_text(&format!("{:#}", anyhow::Error::new(error)))
}

const PLACEMENTS: [Placement; 4] = [
    Placement::SplitRight,
    Placement::SplitDown,
    Placement::NewTab,
    Placement::ReplaceCurrentPane,
];
const PLACEMENT_LABELS: [&str; 4] = [
    "Split right",
    "Split down",
    "New tab",
    "Replace current pane (closes it after verified attach)",
];

fn placement_index(placement: Placement) -> usize {
    match placement {
        Placement::SplitRight => 0,
        Placement::SplitDown => 1,
        Placement::NewTab => 2,
        Placement::ReplaceCurrentPane => 3,
    }
}

/// Runs the interactive terminal explorer. Escape and Ctrl-C return `Ok(None)`
/// except while an explicitly confirmed close is pending; that bounded action
/// completes before exit. Terminal modes are always restored before return.
pub fn run_picker(
    options: PickerOptions,
    status_service: StatusService,
    discovery_service: DiscoveryService,
    lifecycle_service: LifecycleService,
    prune_service: PruneService,
    retention_days: u64,
) -> Result<Option<PickerSelection>> {
    run_picker_with_operation_error(
        options,
        status_service,
        discovery_service,
        lifecycle_service,
        prune_service,
        retention_days,
        None,
    )
}

pub fn run_picker_with_operation_error(
    options: PickerOptions,
    status_service: StatusService,
    discovery_service: DiscoveryService,
    lifecycle_service: LifecycleService,
    prune_service: PruneService,
    retention_days: u64,
    operation_error: Option<(PickerSelection, String)>,
) -> Result<Option<PickerSelection>> {
    let mut state = PickerState::with_retention(options, retention_days)?;
    if let Some((attempted, diagnostic)) = operation_error {
        state.set_operation_error(attempted, diagnostic);
    }
    enable_raw_mode().context("enable terminal raw mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("enter terminal alternate screen");
    }

    let result = run_terminal_picker(
        &mut state,
        &status_service,
        &discovery_service,
        &lifecycle_service,
        &prune_service,
    );
    finish_terminal_session(
        result,
        || execute!(io::stdout(), LeaveAlternateScreen, Show).context("restore terminal screen"),
        || disable_raw_mode().context("disable terminal raw mode"),
    )
}

fn finish_terminal_session<T>(
    picker_result: Result<T>,
    restore_screen: impl FnOnce() -> Result<()>,
    disable_raw: impl FnOnce() -> Result<()>,
) -> Result<T> {
    let screen_result = restore_screen();
    let raw_result = disable_raw();
    match (picker_result, screen_result, raw_result) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (picker_result, screen_result, raw_result) => {
            let mut failures = Vec::new();
            if let Err(error) = picker_result {
                failures.push(format!("picker: {error:#}"));
            }
            if let Err(error) = screen_result {
                failures.push(format!("screen cleanup: {error:#}"));
            }
            if let Err(error) = raw_result {
                failures.push(format!("terminal mode cleanup: {error:#}"));
            }
            Err(anyhow::anyhow!(failures.join("; ")))
        }
    }
}

fn start_status_run(
    state: &mut PickerState,
    service: &StatusService,
    generation: u64,
) -> StatusRun {
    state.begin_refresh(generation);
    service.start(state.status_request())
}

fn start_discovery_run(
    state: &mut PickerState,
    service: &DiscoveryService,
    generation: u64,
) -> DiscoveryRun {
    state.begin_discovery(generation);
    service.start(state.discovery_request())
}

fn run_terminal_picker(
    state: &mut PickerState,
    status_service: &StatusService,
    discovery_service: &DiscoveryService,
    lifecycle_service: &LifecycleService,
    prune_service: &PruneService,
) -> Result<Option<PickerSelection>> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize terminal picker")?;
    terminal.clear().context("clear terminal picker")?;
    let mut generation = 1_u64;
    let mut status_run = start_status_run(state, status_service, generation);
    let mut discovery_run = start_discovery_run(state, discovery_service, generation);
    let mut dirty = true;
    let (close_sender, close_receiver) = mpsc::channel::<PickerCloseResult>();
    let (prune_preview_sender, prune_preview_receiver) = mpsc::channel::<PickerPruneResult>();
    let (prune_apply_sender, prune_apply_receiver) = mpsc::channel::<PickerPruneResult>();

    loop {
        while let Ok(message) = status_run.receiver.try_recv() {
            let missing_id = match &message {
                StatusMessage::Workload {
                    id,
                    status: WorkloadStatus::Missing,
                    ..
                } => Some(*id),
                _ => None,
            };
            dirty |= state.apply_status(message);
            if let Some(id) = missing_id
                && lifecycle_service.observe_owned(id).is_ok()
                && let Ok(Some(record)) = lifecycle_service.owned_record(id)
            {
                state.reconcile_owned_record(&record);
                dirty = true;
            }
        }
        while let Ok(message) = discovery_run.receiver.try_recv() {
            dirty |= state.apply_discovery(message);
        }
        while let Ok(result) = close_receiver.try_recv() {
            if state.apply_close_result(result) {
                status_run.cancel();
                discovery_run.cancel();
                generation = generation
                    .checked_add(1)
                    .context("close refresh generation overflow")?;
                status_run = start_status_run(state, status_service, generation);
                discovery_run = start_discovery_run(state, discovery_service, generation);
                dirty = true;
            }
        }
        while let Ok(result) = prune_preview_receiver.try_recv() {
            dirty |= state.apply_prune_result(result);
        }
        while let Ok(result) = prune_apply_receiver.try_recv() {
            dirty |= state.apply_prune_result(result);
        }
        if dirty {
            terminal
                .draw(|frame| render_picker(frame, state))
                .context("draw terminal picker")?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50)).context("poll terminal picker input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read terminal picker input")? else {
            dirty = true;
            continue;
        };
        let Some(picker_event) = map_key_with_modals(
            key,
            &state.input,
            state.stage,
            state.close_modal.is_some(),
            state.prune_modal.is_some(),
        ) else {
            continue;
        };
        dirty = true;
        match state.handle(picker_event) {
            PickerOutcome::Continue => {}
            PickerOutcome::RefreshRequested => {
                status_run.cancel();
                discovery_run.cancel();
                generation = generation
                    .checked_add(1)
                    .context("status refresh generation overflow")?;
                status_run = start_status_run(state, status_service, generation);
                discovery_run = start_discovery_run(state, discovery_service, generation);
            }
            PickerOutcome::CloseOwnedRequested {
                id,
                generation,
                action,
            } => {
                let service = lifecycle_service.clone();
                let sender = close_sender.clone();
                thread::spawn(move || {
                    let mut error = match action {
                        PickerCloseAction::Stop => service.close_owned(id).map(|_| ()),
                        PickerCloseAction::Remove => service.remove_owned(id).map(|_| ()),
                    }
                    .err()
                    .map(format_close_error);
                    let record = match service.owned_record(id) {
                        Ok(record) => record,
                        Err(read_error) => {
                            let read_error = format_close_error(read_error);
                            error = Some(match error {
                                Some(close_error) => {
                                    format!(
                                        "{close_error}; authoritative state reread failed: {read_error}"
                                    )
                                }
                                None => format!("authoritative state reread failed: {read_error}"),
                            });
                            None
                        }
                    };
                    let _ = sender.send(PickerCloseResult {
                        id,
                        generation,
                        error,
                        record,
                    });
                });
            }
            PickerOutcome::PrunePreviewRequested {
                older_than_days,
                generation,
            } => {
                let service = prune_service.clone();
                let sender = prune_preview_sender.clone();
                thread::spawn(move || {
                    let result = service.preview(older_than_days).map_err(format_prune_error);
                    let _ = sender.send(PickerPruneResult::Preview { generation, result });
                });
            }
            PickerOutcome::PruneApplyRequested {
                preview,
                generation,
            } => {
                let service = prune_service.clone();
                let sender = prune_apply_sender.clone();
                thread::spawn(move || {
                    let result = service.apply(&preview);
                    let message = match result {
                        Ok(result) => PickerPruneResult::Apply {
                            generation,
                            preview,
                            removed_ids: Some(result.removed_ids),
                            skipped_ids: Some(result.skipped_ids),
                            error: None,
                        },
                        Err(error) => PickerPruneResult::Apply {
                            generation,
                            preview,
                            removed_ids: None,
                            skipped_ids: None,
                            error: Some(format_prune_error(error)),
                        },
                    };
                    let _ = sender.send(message);
                });
            }
            PickerOutcome::Selected(selection) => return Ok(Some(selection)),
            PickerOutcome::Cancelled => return Ok(None),
        }
    }
}

#[cfg(test)]
fn map_key(
    key: KeyEvent,
    input: &PickerInput,
    stage: PickerStage,
    close_modal: bool,
) -> Option<PickerEvent> {
    map_key_with_modals(key, input, stage, close_modal, false)
}

fn map_key_with_modals(
    key: KeyEvent,
    input: &PickerInput,
    stage: PickerStage,
    close_modal: bool,
    prune_modal: bool,
) -> Option<PickerEvent> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(PickerEvent::Cancel);
    }
    if prune_modal {
        if key.kind != KeyEventKind::Press
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        return match key.code {
            KeyCode::Char('y' | 'Y') => Some(PickerEvent::ConfirmPrune),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(PickerEvent::DismissPrune),
            _ => None,
        };
    }
    if close_modal {
        if key.kind != KeyEventKind::Press
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        return match key.code {
            KeyCode::Char('y' | 'Y') => Some(PickerEvent::ConfirmClose),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(PickerEvent::DismissClose),
            _ => None,
        };
    }
    if input != &PickerInput::None {
        return match key.code {
            KeyCode::Esc => Some(PickerEvent::ExitInput),
            KeyCode::Enter => Some(PickerEvent::SubmitInput),
            KeyCode::Backspace => Some(PickerEvent::Delete),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(PickerEvent::Insert(character))
            }
            _ => None,
        };
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }

    if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Char('o' | 'O')) {
        return Some(PickerEvent::ManageObservers);
    }

    match key.code {
        KeyCode::Esc if stage == PickerStage::Host => Some(PickerEvent::Cancel),
        KeyCode::Esc => Some(PickerEvent::Back),
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Some(PickerEvent::Previous),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Some(PickerEvent::Next),
        KeyCode::Backspace => Some(PickerEvent::Back),
        KeyCode::Enter => Some(PickerEvent::Confirm),
        KeyCode::Char('/') if stage == PickerStage::Directory => Some(PickerEvent::BeginFilter),
        KeyCode::Char('P') => None,
        KeyCode::Char('p') if stage == PickerStage::Directory => Some(PickerEvent::BeginPath),
        KeyCode::Char('x' | 'X')
            if stage == PickerStage::Resource
                && key.kind == KeyEventKind::Press
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(PickerEvent::Close)
        }
        KeyCode::Char('r' | 'R')
            if key.kind == KeyEventKind::Press
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(PickerEvent::Refresh)
        }
        _ => None,
    }
}

fn render_picker(frame: &mut Frame<'_>, state: &PickerState) {
    render_picker_with_color_mode(frame, state, std::env::var_os("NO_COLOR").is_none());
}

fn render_picker_with_color_mode(frame: &mut Frame<'_>, state: &PickerState, colors_enabled: bool) {
    let compact_guidance = state.close_modal.is_some()
        || state.pending_close.is_some()
        || state.prune_modal.is_some()
        || state.pending_prune.is_some()
        || state.operation_error.is_some();
    let narrow_guidance = compact_guidance && frame.area().width == NARROW_GUIDANCE_WIDTH;
    let minimum_width = if narrow_guidance {
        NARROW_GUIDANCE_WIDTH
    } else {
        MIN_PICKER_WIDTH
    };
    if frame.area().width < minimum_width || frame.area().height < MIN_PICKER_HEIGHT {
        frame.render_widget(Clear, frame.area());
        frame.render_widget(
            Paragraph::new(PICKER_RESIZE_MESSAGE)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            frame.area(),
        );
        return;
    }
    let labels = state.item_labels();
    let visible_rows = labels.len().min(10) as u16;
    let footer = state.footer_text();
    let panel_width = 72.min(frame.area().width);
    let footer_width = panel_width.saturating_sub(6).max(1);
    let desired_footer_height = wrapped_line_count(&footer, footer_width);
    let area = centered_rect(
        frame.area(),
        72,
        visible_rows
            .saturating_add(desired_footer_height)
            .saturating_add(4)
            .max(9),
    );
    let footer_height = desired_footer_height.min(area.height.saturating_sub(4));
    frame.render_widget(Clear, area);
    let destructive = compact_guidance;
    let accent = if !colors_enabled {
        Color::Reset
    } else if destructive {
        Color::Red
    } else {
        Color::Cyan
    };
    let selected_text = if colors_enabled {
        Color::White
    } else {
        Color::Reset
    };
    let secondary_text = if colors_enabled {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let block = Block::default()
        .title(format!(" Tether · {} ", state.frame_title()))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent));
    frame.render_widget(block, area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
        .split(inner);

    let selected = state.current_index();
    let has_selection = state.current_len() > 0;
    let list_height = chunks[0].height.saturating_sub(1);
    let max_rows = usize::from(list_height);
    let (start, metadata) = viewport_metadata(selected, labels.len(), max_rows);
    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(chunks[0]);
    frame.render_widget(
        Paragraph::new(metadata).style(Style::default().fg(secondary_text)),
        list_chunks[0],
    );
    let lines: Vec<Line<'_>> = labels
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .map(|(index, label)| {
            let safe_label = terminal_safe_text(label);
            let label = bounded_label(
                &safe_label,
                usize::from(list_chunks[1].width.saturating_sub(2)),
            );
            if has_selection && index == selected {
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(accent)),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(selected_text)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![Span::raw("  "), Span::styled(label, secondary_text)])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_chunks[1]);
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(if !colors_enabled {
                Color::Reset
            } else if destructive {
                Color::Red
            } else {
                Color::DarkGray
            }))
            .wrap(Wrap { trim: true }),
        chunks[1],
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

pub fn render_picker_to_text(width: u16, height: u16, state: &PickerState) -> Result<String> {
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).context("initialize picker text renderer")?;
    terminal
        .draw(|frame| render_picker_with_color_mode(frame, state, false))
        .context("draw picker text renderer")?;
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
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

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let lines = text
        .split('\n')
        .map(|line| wrapped_single_line_count(line, width))
        .sum::<usize>()
        .max(1);
    lines.min(usize::from(u16::MAX)) as u16
}

fn wrapped_single_line_count(text: &str, width: usize) -> usize {
    let mut lines = 0_usize;
    let mut occupied = 0_usize;
    for word in text.split_whitespace() {
        let mut word_width = Line::from(word).width();
        if word_width > width {
            if occupied > 0 {
                lines += 1;
            }
            lines += word_width / width;
            word_width %= width;
            occupied = word_width;
        } else if occupied == 0 {
            occupied = word_width;
        } else if occupied + 1 + word_width <= width {
            occupied += 1 + word_width;
        } else {
            lines += 1;
            occupied = word_width;
        }
    }
    lines.saturating_add(usize::from(occupied > 0)).max(1)
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn picker_label_truncation_preserves_extended_graphemes() {
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
    use super::*;

    #[test]
    fn refresh_key_maps_only_on_press() {
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            Some(PickerEvent::Refresh)
        );
        assert_eq!(
            map_key(
                KeyEvent::new_with_kind(
                    KeyCode::Char('r'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            None
        );
        assert_eq!(
            map_key(
                KeyEvent::new_with_kind(
                    KeyCode::Char('r'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            None
        );
    }

    #[test]
    fn escape_backs_out_until_the_host_stage_then_closes() {
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::Back)
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            Some(PickerEvent::Cancel)
        );
    }

    #[test]
    fn input_mode_maps_command_keys_to_text() {
        for character in ['r', 'j', 'k', 'p', '/'] {
            assert_eq!(
                map_key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    &PickerInput::Filter(String::new()),
                    PickerStage::Directory,
                    false,
                ),
                Some(PickerEvent::Insert(character))
            );
        }
    }

    #[test]
    fn destructive_keys_are_press_only_and_modal_keys_are_isolated() {
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::Close)
        );
        assert_eq!(
            map_key(
                KeyEvent::new_with_kind(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
                &PickerInput::None,
                PickerStage::Resource,
                false,
            ),
            None
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &PickerInput::Filter(String::new()),
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::Insert('x'))
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                true,
            ),
            Some(PickerEvent::ConfirmClose)
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                true,
            ),
            Some(PickerEvent::DismissClose)
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                true,
            ),
            Some(PickerEvent::DismissClose)
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                true,
            ),
            None
        );
    }

    #[test]
    fn primary_picker_has_no_prune_shortcut() {
        assert_eq!(
            map_key_with_modals(
                KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
                false,
            ),
            None
        );
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char('P'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::ALT),
        ] {
            assert_eq!(
                map_key_with_modals(key, &PickerInput::None, PickerStage::Host, false, false,),
                None
            );
        }
        assert_eq!(
            map_key_with_modals(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Directory,
                false,
                false,
            ),
            Some(PickerEvent::BeginPath)
        );
        assert_eq!(
            map_key_with_modals(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
                true,
            ),
            Some(PickerEvent::ConfirmPrune)
        );
        assert_eq!(
            map_key_with_modals(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                false,
                true,
            ),
            None
        );
    }

    #[test]
    fn observer_manager_shortcut_is_global_obvious_and_press_only() {
        let mut picker = navigation_picker();
        assert!(picker.footer_text().contains("o Observers"));
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::ManageObservers)
        );
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char('o'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT),
        ] {
            assert_eq!(
                map_key(key, &PickerInput::None, PickerStage::Resource, false,),
                None
            );
        }
        assert_eq!(
            picker.handle(PickerEvent::ManageObservers),
            PickerOutcome::Selected(PickerSelection::ManageObservers)
        );
    }

    #[test]
    fn arrows_never_advance_or_go_back() {
        for code in [KeyCode::Left, KeyCode::Right] {
            assert_eq!(
                map_key(
                    KeyEvent::new(code, KeyModifiers::NONE),
                    &PickerInput::None,
                    PickerStage::Host,
                    false,
                ),
                None
            );
        }
        for (code, event) in [
            (KeyCode::Up, PickerEvent::Previous),
            (KeyCode::Down, PickerEvent::Next),
        ] {
            assert_eq!(
                map_key(
                    KeyEvent::new(code, KeyModifiers::NONE),
                    &PickerInput::None,
                    PickerStage::Host,
                    false,
                ),
                Some(event)
            );
        }
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            Some(PickerEvent::Back)
        );
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Host,
                false,
            ),
            Some(PickerEvent::Confirm)
        );
    }
    #[test]
    fn modified_navigation_and_action_keys_do_not_leak_into_picker_controls() {
        for key in [
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        ] {
            assert_eq!(
                map_key(key, &PickerInput::None, PickerStage::Resource, false,),
                None
            );
        }
    }

    fn navigation_picker() -> PickerState {
        PickerState::new(PickerOptions {
            hosts: ["first", "second"]
                .into_iter()
                .map(|name| PickerHost {
                    name: name.to_owned(),
                    label: name.to_owned(),
                    target: None,
                    origin: PickerHostOrigin::Effective,
                    directories: vec!["/work".to_owned()],
                    scan_roots: Vec::new(),
                    commands: vec![PickerCommand::Shell],
                    workloads: Vec::new(),
                    allow_existing: false,
                    allow_create: true,
                })
                .collect(),
            default_placement: Placement::SplitRight,
        })
        .unwrap()
    }

    #[test]
    fn enter_advances_the_arrow_selected_item_and_controls_advertise_it() {
        let mut picker = navigation_picker();
        let controls = picker.footer_text();
        assert!(controls.contains("↑/↓ navigate"));
        assert!(controls.contains("Enter select"));
        assert!(controls.contains("Backspace close"));
        assert!(controls.contains("Esc close"));
        assert!(controls.contains("r Refresh"));
        assert!(!controls.contains("r Retry"));
        assert!(!controls.contains("← back"));

        assert_eq!(picker.handle(PickerEvent::Next), PickerOutcome::Continue);
        assert_eq!(picker.stage(), PickerStage::Host);
        assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
        assert_eq!(picker.stage(), PickerStage::Resource);
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Confirm);
        let PickerOutcome::Selected(PickerSelection::Create(selection)) =
            picker.handle(PickerEvent::Confirm)
        else {
            panic!("Enter should launch the selected create operation");
        };
        assert_eq!(selection.host, "second");
    }

    #[test]
    fn operation_error_is_stable_until_retry_or_cancel() {
        let mut picker = navigation_picker();
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Confirm);
        let PickerOutcome::Selected(selection) = picker.handle(PickerEvent::Confirm) else {
            panic!("Enter at placement should select");
        };

        picker.set_operation_error(selection.clone(), "create failed\u{1b}[31m\nsecond line");
        let visible = picker.operation_error().unwrap().diagnostic.clone();
        assert!(!visible.contains('\u{1b}'));
        assert!(!visible.contains('\n'));
        assert!(picker.footer_text().contains("Enter retry"));
        assert!(picker.footer_text().contains("Backspace/Esc cancel"));

        assert_eq!(picker.handle(PickerEvent::Next), PickerOutcome::Continue);
        assert_eq!(picker.operation_error().unwrap().diagnostic, visible);
        assert_eq!(picker.handle(PickerEvent::Refresh), PickerOutcome::Continue);
        assert_eq!(picker.operation_error().unwrap().diagnostic, visible);
        assert_eq!(
            picker.handle(PickerEvent::Confirm),
            PickerOutcome::Selected(selection.clone())
        );
        assert!(picker.operation_error().is_none());

        picker.set_operation_error(selection, "still failed");
        assert_eq!(picker.handle(PickerEvent::Back), PickerOutcome::Continue);
        assert!(picker.operation_error().is_none());
    }
    #[test]
    fn terminal_cleanup_attempts_both_restorations_after_picker_failure() {
        use std::cell::Cell;

        let screen_restored = Cell::new(false);
        let raw_disabled = Cell::new(false);
        let error = finish_terminal_session::<()>(
            Err(anyhow::anyhow!("picker failed")),
            || {
                screen_restored.set(true);
                Err(anyhow::anyhow!("screen restore failed"))
            },
            || {
                raw_disabled.set(true);
                Err(anyhow::anyhow!("raw restore failed"))
            },
        )
        .unwrap_err();

        assert!(screen_restored.get());
        assert!(raw_disabled.get());
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("picker failed"));
        assert!(diagnostic.contains("screen restore failed"));
        assert!(diagnostic.contains("raw restore failed"));
    }
}

#[cfg(test)]
mod close_render_tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn close_picker() -> (PickerState, SessionId) {
        let id = "tether-0197f198000070008000000000000001".parse().unwrap();
        let label = "Tether · Resume …00000001 · Shell · /srv/app".to_owned();
        let options = PickerOptions {
            hosts: vec![PickerHost {
                name: "build-box".to_owned(),
                label: "build-box".to_owned(),
                origin: PickerHostOrigin::Effective,
                target: None,
                directories: vec!["/srv/app".to_owned()],
                scan_roots: Vec::new(),
                commands: vec![PickerCommand::Shell],
                workloads: vec![PickerWorkload {
                    id,
                    base_label: label.clone(),
                    status: SessionStatus::Running,
                    legacy: false,
                    last_used_at: chrono::Utc::now(),
                    label,
                }],
                allow_existing: true,
                allow_create: true,
            }],
            default_placement: Placement::SplitRight,
        };
        let mut picker = PickerState::new(options).unwrap();
        picker.begin_refresh(9);
        picker.apply_status(StatusMessage::Host {
            generation: 9,
            host: "build-box".to_owned(),
            status: HostReachability::Reachable,
            detail: None,
            checked_at: std::time::SystemTime::now(),
        });
        picker.apply_status(StatusMessage::Workload {
            generation: 9,
            id,
            status: WorkloadStatus::Running { attached: 0 },
            checked_at: std::time::SystemTime::now(),
        });
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Close);
        (picker, id)
    }

    #[test]
    fn resource_hints_match_reachability_and_available_actions() {
        let (mut picker, _) = close_picker();
        picker.handle(PickerEvent::DismissClose);

        let reachable = picker.footer_text();
        assert!(reachable.contains("Enter Open"));
        assert!(reachable.contains("x Stop"));
        assert!(reachable.contains("r Refresh"));
        assert!(!reachable.contains("r Retry"));

        assert!(picker.apply_status(StatusMessage::Host {
            generation: 9,
            host: "build-box".to_owned(),
            status: HostReachability::Unreachable,
            detail: Some("connection refused".to_owned()),
            checked_at: std::time::SystemTime::now(),
        }));
        let unreachable = picker.footer_text();
        assert!(unreachable.contains("r Retry"));
        assert!(!unreachable.contains("Enter Open"));
        assert!(!unreachable.contains("x Stop"));
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn narrow_close_frames_keep_guidance_and_red_destructive_accent_visible() {
        let (mut picker, id) = close_picker();
        let mut terminal = Terminal::new(TestBackend::new(32, 16)).unwrap();

        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Confirm Stop"));
        assert!(rendered.contains("y confirm"));
        assert!(rendered.contains("n/Esc keep"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );

        assert_eq!(
            picker.handle(PickerEvent::ConfirmClose),
            PickerOutcome::CloseOwnedRequested {
                id,
                generation: 9,
                action: PickerCloseAction::Stop
            }
        );
        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Applying lifecycle"));
        assert!(rendered.contains("wait for result"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );

        assert!(picker.apply_close_result(PickerCloseResult {
            id,
            generation: 9,
            error: Some(format!("backend source {}", "x".repeat(500))),
            record: None,
        }));
        let Some(PickerCloseModal::Failed { error, .. }) = picker.close_modal() else {
            panic!("failed close modal missing");
        };
        assert!(error.chars().count() <= 241);
        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Stop failed"));
        assert!(rendered.contains("y retry"));
        assert!(rendered.contains("n/Esc dismiss"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );
    }

    #[test]
    fn failed_close_retry_keeps_the_confirmed_action_when_status_changes() {
        let (mut picker, id) = close_picker();
        assert_eq!(
            picker.handle(PickerEvent::ConfirmClose),
            PickerOutcome::CloseOwnedRequested {
                id,
                generation: 9,
                action: PickerCloseAction::Stop,
            }
        );
        picker.options.hosts[0].workloads[0].status = SessionStatus::Ended;

        assert!(picker.apply_close_result(PickerCloseResult {
            id,
            generation: 9,
            error: Some("transport failed after the workload ended".to_owned()),
            record: None,
        }));

        assert_eq!(picker.frame_title(), "Stop failed");
        assert!(picker.footer_text().contains("Stop failed"));
        assert_eq!(
            picker.handle(PickerEvent::ConfirmClose),
            PickerOutcome::CloseOwnedRequested {
                id,
                generation: 9,
                action: PickerCloseAction::Stop,
            }
        );
    }

    fn prune_preview() -> PrunePreview {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::state::StateStore::new(temp.path().join("state.json"));
        let now = chrono::Utc::now();
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![SessionRecord {
                    id: "tether-0197f198000070008000000000000099".parse().unwrap(),
                    host: "old".to_owned(),
                    target: "old.example".to_owned(),
                    directory: "/old".to_owned(),
                    preset: None,
                    command: Some("exec shell".to_owned()),
                    tmux_session_id: None,
                    ownership_proof: None,
                    status: SessionStatus::Ended,
                    created_at: now - chrono::Duration::days(40),
                    last_used_at: now - chrono::Duration::days(40),
                    closed_at: Some(now - chrono::Duration::days(40)),
                    exit_status: None,
                }],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        PruneService::new(store).preview(14).unwrap()
    }

    #[test]
    fn narrow_prune_frames_prioritize_guidance_and_red_destructive_accent() {
        let (mut picker, _) = close_picker();
        picker.close_modal = None;
        picker.retention_days = 14;
        let preview = prune_preview();
        let mut terminal = Terminal::new(TestBackend::new(32, 16)).unwrap();

        assert!(matches!(
            picker.handle(PickerEvent::BeginPrune),
            PickerOutcome::PrunePreviewRequested { .. }
        ));
        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let pending = rendered_text(&terminal)
            .replace(['│', '┌', '─', '┐', '└', '┘'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(pending.contains("Previewing prune"));
        assert!(pending.contains("No host contact"));

        assert!(picker.apply_prune_result(PickerPruneResult::Preview {
            generation: 9,
            result: Ok(preview.clone()),
        }));
        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let confirm = rendered_text(&terminal)
            .replace(['│', '┌', '─', '┐', '└', '┘'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(confirm.contains("Confirm prune"));
        assert!(confirm.contains("1 closed metadata"));
        assert!(confirm.contains("14 days"));
        assert!(confirm.contains("y confirm"));
        assert!(confirm.contains("n/Esc keep"));
        assert!(confirm.contains("No host contact"));

        picker.handle(PickerEvent::ConfirmPrune);
        assert!(picker.apply_prune_result(PickerPruneResult::Apply {
            generation: 9,
            preview,
            removed_ids: None,
            skipped_ids: None,
            error: Some("persistence unavailable".to_owned()),
        }));
        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let failed = rendered_text(&terminal)
            .replace(['│', '┌', '─', '┐', '└', '┘'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(failed.contains("Prune failed"));
        assert!(failed.contains("y retry"));
        assert!(failed.contains("n/Esc dismiss"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );
    }
    #[test]
    fn operation_error_renders_safe_diagnostic_and_explicit_actions() {
        let (mut picker, id) = close_picker();
        picker.set_operation_error(
            PickerSelection::Resume {
                id,
                placement: Placement::SplitRight,
            },
            "placement unavailable",
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();

        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();
        let rendered = rendered_text(&terminal)
            .replace(['│', '┌', '─', '┐', '└', '┘'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("Operation failed"));
        assert!(rendered.contains("placement unavailable"));
        assert!(rendered.contains("Enter retry"));
        assert!(rendered.contains("Backspace/Esc cancel"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );
    }
    #[test]
    fn monochrome_render_keeps_selection_and_destructive_guidance_textual() {
        let (picker, _) = close_picker();
        let mut terminal = Terminal::new(TestBackend::new(32, 8)).unwrap();

        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, false))
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains('>'));
        assert!(rendered.contains("y confirm"));
        assert!(rendered.contains("n/Esc keep"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset)
        );
    }

    #[test]
    fn rendered_resource_labels_never_emit_terminal_control_sequences() {
        let (mut picker, _) = close_picker();
        let unsafe_label = "workload\u{1b}]0;spoof\u{7}\nnext".to_owned();
        picker.options.hosts[0].workloads[0].label = unsafe_label.clone();
        picker.options.hosts[0].workloads[0].base_label = unsafe_label;
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();

        terminal
            .draw(|frame| render_picker_with_color_mode(frame, &picker, true))
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("workload next"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains("spoof"));
    }

    #[test]
    fn tiny_picker_and_modal_geometries_never_panic() {
        for (width, height) in [(1, 1), (5, 3), (20, 5), (39, 8), (40, 7), (40, 8), (80, 24)] {
            let (mut picker, _) = close_picker();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_picker(frame, &picker))
                .unwrap();
            let below_minimum = width < MIN_PICKER_WIDTH || height < MIN_PICKER_HEIGHT;
            if below_minimum && width >= PICKER_RESIZE_MESSAGE.len() as u16 {
                assert!(rendered_text(&terminal).contains(PICKER_RESIZE_MESSAGE));
            } else if !below_minimum {
                let confirmation = rendered_text(&terminal);
                assert!(confirmation.contains("y confirm"), "{width}x{height}");
                assert!(confirmation.contains("n/Esc keep"), "{width}x{height}");
            }
            picker.handle(PickerEvent::DismissClose);
            terminal
                .draw(|frame| render_picker(frame, &picker))
                .unwrap();
            if !below_minimum {
                let picker_text = rendered_text(&terminal);
                assert!(
                    picker_text.contains("Enter Open"),
                    "{width}x{height}: {picker_text:?}"
                );
                assert!(
                    picker_text.contains("Esc back"),
                    "{width}x{height}: {picker_text:?}"
                );
            }
        }
    }
}
