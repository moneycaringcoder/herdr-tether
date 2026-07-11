use std::{
    collections::{HashMap, HashSet},
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
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{
    config::Config,
    discovery::{
        DiscoveryCompletion, DiscoveryLocation, DiscoveryMessage, DiscoveryRequest, DiscoveryRun,
        DiscoveryService,
    },
    lifecycle::{CloseOwnedError, LifecycleService},
    model::{ExternalSessionName, Placement, SessionId},
    state::{SessionRecord, SessionStatus, State},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, StatusHost, StatusMessage,
        StatusRequest, StatusRun, StatusService, WorkloadStatus,
    },
};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerWorkload {
    pub id: SessionId,
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
    pub directories: Vec<String>,
    pub scan_roots: Vec<String>,
    pub commands: Vec<PickerCommand>,
    pub workloads: Vec<PickerWorkload>,
    pub allow_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerOptions {
    pub hosts: Vec<PickerHost>,
    pub default_placement: Placement,
}

impl PickerOptions {
    /// Builds picker data with recent session directories first, followed by
    /// configured roots. Duplicate hosts and directories retain their first,
    /// highest-precedence occurrence.
    pub fn from_config_state(
        config: &Config,
        state: &State,
        local_directory: &str,
        include_local: bool,
    ) -> Self {
        let mut hosts = Vec::with_capacity(config.hosts.len() + usize::from(include_local));
        let mut host_names = HashSet::with_capacity(hosts.capacity());

        if include_local {
            host_names.insert("local".to_owned());
            let mut scan_roots = config
                .discovery
                .local_roots
                .iter()
                .map(|root| expand_local_root(root, local_directory))
                .collect::<Vec<_>>();
            if scan_roots.is_empty() {
                scan_roots.push(local_directory.to_owned());
            }
            let mut directories = recent_directories(state, "local");
            for root in &scan_roots {
                push_unique(&mut directories, root);
            }
            hosts.push(PickerHost {
                name: "local".to_owned(),
                label: "local".to_owned(),
                target: None,
                directories,
                scan_roots,
                commands: vec![PickerCommand::Shell],
                workloads: active_workloads(state, "local"),
                allow_existing: true,
            });
        }

        for host in &config.hosts {
            if !host_names.insert(host.name.clone()) {
                continue;
            }
            let mut scan_roots = host.roots.clone();
            if scan_roots.is_empty() {
                scan_roots.push("~".to_owned());
            }
            let mut directories = recent_directories(state, &host.name);
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
                directories,
                scan_roots,
                commands,
                workloads: active_workloads(state, &host.name),
                allow_existing: true,
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

fn recent_directories(state: &State, host: &str) -> Vec<String> {
    let mut sessions: Vec<&SessionRecord> = state
        .sessions
        .iter()
        .filter(|session| session.host == host)
        .collect();
    sessions.sort_by(|left, right| {
        right
            .last_used_at
            .cmp(&left.last_used_at)
            .then_with(|| left.directory.cmp(&right.directory))
    });

    let mut directories = Vec::with_capacity(sessions.len());
    for session in sessions {
        push_unique(&mut directories, &session.directory);
    }
    directories
}
fn active_workloads(state: &State, host: &str) -> Vec<PickerWorkload> {
    let mut sessions: Vec<&SessionRecord> = state
        .sessions
        .iter()
        .filter(|session| session.host == host && session.status == SessionStatus::Active)
        .collect();
    sessions.sort_by(|left, right| {
        right
            .last_used_at
            .cmp(&left.last_used_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    sessions
        .into_iter()
        .map(|session| {
            let command = session.preset.as_deref().unwrap_or("Shell");
            let id = session.id.to_string();
            let short_id = &id[id.len().saturating_sub(8)..];
            let label = format!(
                "Tether · Resume …{} · {} · {}",
                short_id, command, session.directory
            );
            PickerWorkload {
                id: session.id,
                base_label: label.clone(),
                label,
            }
        })
        .collect()
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerSelection {
    Create(OpenSelection),
    Resume {
        id: SessionId,
        placement: Placement,
    },
    AttachExternal {
        host: String,
        target: Option<String>,
        name: ExternalSessionName,
        placement: Placement,
    },
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
    Close,
    ConfirmClose,
    DismissClose,
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
    CloseOwnedRequested { id: SessionId, generation: u64 },
    Selected(PickerSelection),
    Cancelled,
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
    external_name: Option<ExternalSessionName>,
    generation: u64,
    host_status: HashMap<String, StatusCell<HostReachability>>,
    workload_status: HashMap<SessionId, StatusCell<WorkloadStatus>>,
    catalogs: HashMap<String, CatalogCell>,
    discovery_generation: u64,
    base_directories: HashMap<String, Vec<String>>,
    discovery: HashMap<String, DiscoveryView>,
    directory_filter: String,
    input: PickerInput,
    selected_directory: Option<String>,
    close_modal: Option<PickerCloseModal>,
    pending_close: Option<(SessionId, u64)>,
    close_failed: HashMap<SessionId, String>,
}

impl PickerState {
    pub fn new(options: PickerOptions) -> Result<Self> {
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
            external_name: None,
            generation: 0,
            host_status: HashMap::new(),
            workload_status: HashMap::new(),
            catalogs: HashMap::new(),
            discovery_generation: 0,
            base_directories,
            discovery: HashMap::new(),
            directory_filter: String::new(),
            input: PickerInput::None,
            selected_directory: None,
            close_modal: None,
            pending_close: None,
            close_failed: HashMap::new(),
        })
    }

    pub fn stage(&self) -> PickerStage {
        self.stage
    }

    pub fn input(&self) -> &PickerInput {
        &self.input
    }

    pub fn close_modal(&self) -> Option<&PickerCloseModal> {
        self.close_modal.as_ref()
    }

    pub fn close_busy(&self) -> bool {
        self.pending_close.is_some()
    }

    pub fn apply_close_result(&mut self, result: PickerCloseResult) -> bool {
        if result.generation != self.generation
            || self.pending_close.as_ref() != Some(&(result.id, result.generation))
        {
            return false;
        }
        self.pending_close = None;
        if let Some(error) = result.error {
            let error = bounded_error_text(&sanitize_terminal_text(&error));
            self.close_failed.insert(result.id, error.clone());
            self.close_modal = Some(PickerCloseModal::Failed {
                id: result.id,
                error,
            });
            self.rebuild_status_labels();
            return true;
        }

        self.close_failed.remove(&result.id);
        self.close_modal = None;
        for host_index in 0..self.options.hosts.len() {
            let Some(workload_index) = self.options.hosts[host_index]
                .workloads
                .iter()
                .position(|workload| workload.id == result.id)
            else {
                continue;
            };
            self.options.hosts[host_index]
                .workloads
                .remove(workload_index);
            self.workload_status.remove(&result.id);
            if host_index == self.host_index {
                if self.resource_index > workload_index {
                    self.resource_index -= 1;
                } else if self.resource_index == workload_index {
                    self.resource_index = workload_index.min(self.resource_len().saturating_sub(1));
                }
            }
            return true;
        }
        true
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
        for host in &mut self.options.hosts {
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
                let Some(host) = self
                    .options
                    .hosts
                    .iter_mut()
                    .find(|candidate| candidate.name == host)
                else {
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
        if let Some(catalog) = self.catalogs.get(&host.name) {
            labels.extend(catalog.sessions.iter().map(|session| session.label.clone()));
        }
        labels.push("Create new Tether workload".to_owned());
        Some(labels)
    }

    pub fn begin_refresh(&mut self, generation: u64) {
        self.generation = generation;
        if self
            .pending_close
            .as_ref()
            .is_some_and(|(_, pending_generation)| *pending_generation != generation)
        {
            self.pending_close = None;
        }
        for host in &self.options.hosts {
            self.host_status
                .entry(host.name.clone())
                .or_default()
                .begin_refresh();
            for workload in &host.workloads {
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
                checked_at,
                ..
            } => self.host_status.get_mut(&host).is_some_and(|cell| {
                cell.apply(status, checked_at);
                true
            }),
            StatusMessage::Workload {
                id,
                status,
                checked_at,
                ..
            } => self.workload_status.get_mut(&id).is_some_and(|cell| {
                cell.apply(status, checked_at);
                true
            }),
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
        if let Some(session) = self
            .catalogs
            .get(&host.name)
            .and_then(|catalog| catalog.sessions.get(external_index))
        {
            return Some(ResourceIdentity::External(session.name.clone()));
        }
        Some(ResourceIdentity::Create)
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
            + self
                .catalogs
                .get(&host.name)
                .map_or(0, |catalog| catalog.sessions.len())
            + 1
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
                    workloads: host.workloads.iter().map(|workload| workload.id).collect(),
                })
                .collect(),
        }
    }

    fn rebuild_status_labels(&mut self) {
        for host in &mut self.options.hosts {
            host.label = format_status_label(
                &host.name,
                self.host_status.get(&host.name),
                host_status_text,
            );
            for workload in &mut host.workloads {
                workload.label = format_status_label(
                    &workload.base_label,
                    self.workload_status.get(&workload.id),
                    workload_status_text,
                );
                if self
                    .pending_close
                    .as_ref()
                    .is_some_and(|(id, _)| *id == workload.id)
                {
                    workload.label = format!("[closing…] {}", workload.base_label);
                } else if self.close_failed.contains_key(&workload.id) {
                    workload.label = format!("[close failed · c retry] {}", workload.base_label);
                }
            }
        }
    }

    pub fn handle(&mut self, event: PickerEvent) -> PickerOutcome {
        if self.pending_close.is_some()
            && !matches!(event, PickerEvent::Previous | PickerEvent::Next)
        {
            return PickerOutcome::Continue;
        }
        if matches!(event, PickerEvent::Cancel) {
            return PickerOutcome::Cancelled;
        }
        if self.close_modal.is_some() {
            return match event {
                PickerEvent::DismissClose => {
                    self.close_modal = None;
                    PickerOutcome::Continue
                }
                PickerEvent::ConfirmClose => self.confirm_close(),
                _ => PickerOutcome::Continue,
            };
        }
        if self.input != PickerInput::None {
            return self.handle_input(event);
        }
        match event {
            PickerEvent::Refresh if self.pending_close.is_none() => PickerOutcome::RefreshRequested,
            PickerEvent::Refresh => PickerOutcome::Continue,
            PickerEvent::Close => self.begin_close(),
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
            | PickerEvent::BeginFilter
            | PickerEvent::BeginPath
            | PickerEvent::Insert(_)
            | PickerEvent::Delete
            | PickerEvent::SubmitInput
            | PickerEvent::ExitInput => PickerOutcome::Continue,
        }
    }

    fn begin_close(&mut self) -> PickerOutcome {
        if self.stage != PickerStage::Resource || self.pending_close.is_some() {
            return PickerOutcome::Continue;
        }
        let Some(ResourceIdentity::Owned(id)) = self.current_resource_identity() else {
            return PickerOutcome::Continue;
        };
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
        if !self
            .options
            .hosts
            .iter()
            .any(|host| host.workloads.iter().any(|workload| workload.id == id))
        {
            self.close_modal = None;
            return PickerOutcome::Continue;
        }
        self.close_modal = None;
        self.pending_close = Some((id, self.generation));
        self.rebuild_status_labels();
        PickerOutcome::CloseOwnedRequested {
            id,
            generation: self.generation,
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
            PickerStage::Placement if self.resume_id.is_some() || self.external_name.is_some() => {
                PickerStage::Resource
            }
            PickerStage::Placement => PickerStage::Command,
        };
        self.resume_id = None;
        self.external_name = None;
        PickerOutcome::Continue
    }

    fn confirm(&mut self) -> PickerOutcome {
        match self.stage {
            PickerStage::Host => {
                self.resource_index = 0;
                self.directory_index = 0;
                self.command_index = 0;
                self.resume_id = None;
                self.external_name = None;
                self.selected_directory = None;
                self.directory_filter.clear();
                self.stage = PickerStage::Resource;
                PickerOutcome::Continue
            }
            PickerStage::Resource => {
                let host = &self.options.hosts[self.host_index];
                if let Some(workload) = host.workloads.get(self.resource_index) {
                    if self
                        .pending_close
                        .as_ref()
                        .is_some_and(|(id, _)| *id == workload.id)
                        || self.close_failed.contains_key(&workload.id)
                    {
                        return PickerOutcome::Continue;
                    }
                    let missing = self.workload_status.get(&workload.id).is_some_and(|cell| {
                        !cell.stale && matches!(cell.value, Some(WorkloadStatus::Missing))
                    });
                    if missing {
                        return PickerOutcome::Continue;
                    }
                    self.resume_id = Some(workload.id);
                    self.external_name = None;
                    self.stage = PickerStage::Placement;
                    return PickerOutcome::Continue;
                }
                let external_index = self.resource_index.saturating_sub(host.workloads.len());
                if let Some(session) = self
                    .catalogs
                    .get(&host.name)
                    .and_then(|catalog| catalog.sessions.get(external_index))
                {
                    self.resume_id = None;
                    self.external_name = Some(session.name.clone());
                    self.stage = PickerStage::Placement;
                } else {
                    self.resume_id = None;
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
                host.workloads
                    .iter()
                    .map(|workload| workload.label.as_str())
                    .chain(
                        self.catalogs
                            .get(&host.name)
                            .into_iter()
                            .flat_map(|catalog| &catalog.sessions)
                            .map(|session| session.label.as_str()),
                    )
                    .chain(std::iter::once("Create new Tether workload"))
                    .collect()
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

    fn modal_text(&self) -> Option<String> {
        match self.close_modal.as_ref()? {
            PickerCloseModal::Confirm { id } => Some(format!(
                "y confirm · n/Esc keep · Close exact {id}? Terminates its tmux session."
            )),
            PickerCloseModal::Failed { error, .. } => {
                Some(format!("y retry · n/Esc dismiss · Close failed: {error}"))
            }
        }
    }

    pub fn frame_title(&self) -> String {
        match self.close_modal.as_ref() {
            Some(PickerCloseModal::Confirm { .. }) => "Confirm close".to_owned(),
            Some(PickerCloseModal::Failed { .. }) => "Close failed".to_owned(),
            None if self.pending_close.is_some() => "Closing workload".to_owned(),
            None => self.title().to_owned(),
        }
    }

    pub fn footer_text(&self) -> String {
        if let Some((id, _)) = self.pending_close {
            return format!("Closing · wait for result · {id} · ↑/↓ navigate");
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
                    && let Some(label) =
                        self.catalog_label(&self.options.hosts[self.host_index].name)
                {
                    parts.push(label);
                }
                let close_hint = if self.stage == PickerStage::Resource
                    && matches!(
                        self.current_resource_identity(),
                        Some(ResourceIdentity::Owned(_))
                    ) {
                    " · c close"
                } else {
                    ""
                };
                parts.push(format!(
                    "↑/↓ navigate · Enter select{close_hint} · / filter · p path · r refresh · ← back · Esc cancel"
                ));
                parts.join(" · ")
            }
        }
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

const PLACEMENTS: [Placement; 3] = [
    Placement::SplitRight,
    Placement::SplitDown,
    Placement::NewTab,
];
const PLACEMENT_LABELS: [&str; 3] = ["Split right", "Split down", "New tab"];

fn placement_index(placement: Placement) -> usize {
    match placement {
        Placement::SplitRight => 0,
        Placement::SplitDown => 1,
        Placement::NewTab => 2,
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
) -> Result<Option<PickerSelection>> {
    let mut state = PickerState::new(options)?;
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
    );
    let leave_result =
        execute!(io::stdout(), LeaveAlternateScreen, Show).context("restore terminal screen");
    let raw_result = disable_raw_mode().context("disable terminal raw mode");

    match result {
        Err(error) => Err(error),
        Ok(selection) => {
            leave_result?;
            raw_result?;
            Ok(selection)
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
) -> Result<Option<PickerSelection>> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize terminal picker")?;
    terminal.clear().context("clear terminal picker")?;
    let mut generation = 1_u64;
    let mut status_run = start_status_run(state, status_service, generation);
    let mut discovery_run = start_discovery_run(state, discovery_service, generation);
    let mut dirty = true;
    let (close_sender, close_receiver) = mpsc::channel::<PickerCloseResult>();

    loop {
        while let Ok(message) = status_run.receiver.try_recv() {
            dirty |= state.apply_status(message);
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
        let Some(picker_event) =
            map_key(key, &state.input, state.stage, state.close_modal.is_some())
        else {
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
            PickerOutcome::CloseOwnedRequested { id, generation } => {
                let service = lifecycle_service.clone();
                let sender = close_sender.clone();
                thread::spawn(move || {
                    let error = service.close_owned(id).err().map(format_close_error);
                    let _ = sender.send(PickerCloseResult {
                        id,
                        generation,
                        error,
                    });
                });
            }
            PickerOutcome::Selected(selection) => return Ok(Some(selection)),
            PickerOutcome::Cancelled => return Ok(None),
        }
    }
}

fn map_key(
    key: KeyEvent,
    input: &PickerInput,
    stage: PickerStage,
    close_modal: bool,
) -> Option<PickerEvent> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(PickerEvent::Cancel);
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
    match key.code {
        KeyCode::Esc => Some(PickerEvent::Cancel),
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Some(PickerEvent::Previous),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Some(PickerEvent::Next),
        KeyCode::Backspace | KeyCode::Left => Some(PickerEvent::Back),
        KeyCode::Enter | KeyCode::Right => Some(PickerEvent::Confirm),
        KeyCode::Char('/') if stage == PickerStage::Directory => Some(PickerEvent::BeginFilter),
        KeyCode::Char('p' | 'P') if stage == PickerStage::Directory => Some(PickerEvent::BeginPath),
        KeyCode::Char('c' | 'C')
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
    let labels = state.item_labels();
    let visible_rows = labels.len().min(10) as u16;
    let footer = state.footer_text();
    let panel_width = 72.min(frame.area().width);
    let footer_width = panel_width.saturating_sub(6).max(1);
    let footer_height = wrapped_line_count(&footer, footer_width);
    let area = centered_rect(
        frame.area(),
        72,
        visible_rows
            .saturating_add(footer_height)
            .saturating_add(4)
            .max(9),
    );
    frame.render_widget(Clear, area);
    let destructive = state.close_modal.is_some() || state.pending_close.is_some();
    let accent = if destructive { Color::Red } else { Color::Cyan };
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
    let max_rows = chunks[0].height as usize;
    let start = selected.saturating_sub(max_rows.saturating_sub(1));
    let lines: Vec<Line<'_>> = labels
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .map(|(index, label)| {
            if has_selection && index == selected {
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(accent)),
                    Span::styled(
                        *label,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![Span::raw("  "), Span::styled(*label, Color::DarkGray)])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), chunks[0]);
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(if destructive {
                Color::Red
            } else {
                Color::DarkGray
            }))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
}

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
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
    lines
        .saturating_add(usize::from(occupied > 0))
        .max(1)
        .min(usize::from(u16::MAX)) as u16
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
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                &PickerInput::None,
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::Close)
        );
        assert_eq!(
            map_key(
                KeyEvent::new_with_kind(
                    KeyCode::Char('c'),
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
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                &PickerInput::Filter(String::new()),
                PickerStage::Resource,
                false,
            ),
            Some(PickerEvent::Insert('c'))
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
                target: None,
                directories: vec!["/srv/app".to_owned()],
                scan_roots: Vec::new(),
                commands: vec![PickerCommand::Shell],
                workloads: vec![PickerWorkload {
                    id,
                    base_label: label.clone(),
                    label,
                }],
                allow_existing: true,
            }],
            default_placement: Placement::SplitRight,
        };
        let mut picker = PickerState::new(options).unwrap();
        picker.begin_refresh(9);
        picker.handle(PickerEvent::Confirm);
        picker.handle(PickerEvent::Close);
        (picker, id)
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
            .draw(|frame| render_picker(frame, &picker))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Confirm close"));
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
            PickerOutcome::CloseOwnedRequested { id, generation: 9 }
        );
        terminal
            .draw(|frame| render_picker(frame, &picker))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Closing workload"));
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
        }));
        let Some(PickerCloseModal::Failed { error, .. }) = picker.close_modal() else {
            panic!("failed close modal missing");
        };
        assert!(error.chars().count() <= 241);
        terminal
            .draw(|frame| render_picker(frame, &picker))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Close failed"));
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
}
