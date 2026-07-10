use std::{
    collections::{HashMap, HashSet},
    io,
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
    model::{Placement, SessionId},
    state::{SessionRecord, SessionStatus, State},
    status::{
        HostReachability, StatusHost, StatusMessage, StatusRequest, StatusRun, StatusService,
        WorkloadStatus,
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
pub struct PickerHost {
    pub name: String,
    pub label: String,
    pub target: Option<String>,
    pub directories: Vec<String>,
    pub commands: Vec<PickerCommand>,
    pub workloads: Vec<PickerWorkload>,
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
            let mut directories = recent_directories(state, "local");
            push_unique(&mut directories, local_directory);
            if directories.is_empty() {
                directories.push("~".to_owned());
            }
            hosts.push(PickerHost {
                name: "local".to_owned(),
                label: "local".to_owned(),
                target: None,
                directories,
                commands: vec![PickerCommand::Shell],
                workloads: active_workloads(state, "local"),
            });
        }

        for host in &config.hosts {
            if !host_names.insert(host.name.clone()) {
                continue;
            }
            let mut directories = recent_directories(state, &host.name);
            for root in &host.roots {
                push_unique(&mut directories, root);
            }
            // SSH config aliases synthesized by the CLI have no configured
            // roots. `~` keeps them immediately usable without persisting data.
            if directories.is_empty() {
                directories.push("~".to_owned());
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
                commands,
                workloads: active_workloads(state, &host.name),
            });
        }

        Self {
            hosts,
            default_placement: config.ui.placement,
        }
    }
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
            let label = format!("Resume …{} · {} · {}", short_id, command, session.directory);
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
    Resume { id: SessionId, placement: Placement },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerStage {
    Host,
    Resource,
    Directory,
    Command,
    Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerEvent {
    Previous,
    Next,
    Confirm,
    Back,
    Cancel,
    Refresh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerOutcome {
    Continue,
    RefreshRequested,
    Selected(PickerSelection),
    Cancelled,
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
    generation: u64,
    host_status: HashMap<String, StatusCell<HostReachability>>,
    workload_status: HashMap<SessionId, StatusCell<WorkloadStatus>>,
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
            generation: 0,
            host_status: HashMap::new(),
            workload_status: HashMap::new(),
        })
    }

    pub fn stage(&self) -> PickerStage {
        self.stage
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

    pub fn begin_refresh(&mut self, generation: u64) {
        self.generation = generation;
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
        }
        self.rebuild_status_labels();
    }

    pub fn apply_status(&mut self, message: StatusMessage) -> bool {
        if message.generation() != self.generation {
            return false;
        }
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
            StatusMessage::Finished { .. } => false,
        };
        if applied {
            self.rebuild_status_labels();
        }
        applied
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
            }
        }
    }

    pub fn handle(&mut self, event: PickerEvent) -> PickerOutcome {
        match event {
            PickerEvent::Cancel => PickerOutcome::Cancelled,
            PickerEvent::Refresh => PickerOutcome::RefreshRequested,
            PickerEvent::Back => self.back(),
            PickerEvent::Previous => {
                let length = self.current_len();
                let index = self.current_index_mut();
                *index = if *index == 0 { length - 1 } else { *index - 1 };
                PickerOutcome::Continue
            }
            PickerEvent::Next => {
                let length = self.current_len();
                let index = self.current_index_mut();
                *index = (*index + 1) % length;
                PickerOutcome::Continue
            }
            PickerEvent::Confirm => self.confirm(),
        }
    }

    fn back(&mut self) -> PickerOutcome {
        self.stage = match self.stage {
            PickerStage::Host => return PickerOutcome::Cancelled,
            PickerStage::Resource => PickerStage::Host,
            PickerStage::Directory if self.options.hosts[self.host_index].workloads.is_empty() => {
                PickerStage::Host
            }
            PickerStage::Directory => PickerStage::Resource,
            PickerStage::Command => PickerStage::Directory,
            PickerStage::Placement if self.resume_id.is_some() => PickerStage::Resource,
            PickerStage::Placement => PickerStage::Command,
        };
        self.resume_id = None;
        PickerOutcome::Continue
    }

    fn confirm(&mut self) -> PickerOutcome {
        match self.stage {
            PickerStage::Host => {
                self.resource_index = 0;
                self.directory_index = 0;
                self.command_index = 0;
                self.resume_id = None;
                self.stage = if self.options.hosts[self.host_index].workloads.is_empty() {
                    PickerStage::Directory
                } else {
                    PickerStage::Resource
                };
                PickerOutcome::Continue
            }
            PickerStage::Resource => {
                let host = &self.options.hosts[self.host_index];
                if let Some(workload) = host.workloads.get(self.resource_index) {
                    let missing = self.workload_status.get(&workload.id).is_some_and(|cell| {
                        !cell.stale && matches!(cell.value, Some(WorkloadStatus::Missing))
                    });
                    if missing {
                        return PickerOutcome::Continue;
                    }
                    self.resume_id = Some(workload.id);
                    self.stage = PickerStage::Placement;
                } else {
                    self.resume_id = None;
                    self.stage = PickerStage::Directory;
                }
                PickerOutcome::Continue
            }
            PickerStage::Directory => {
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

        let host = &self.options.hosts[self.host_index];
        let (preset, command) = host.commands[self.command_index].selection_parts();
        PickerSelection::Create(OpenSelection {
            host: host.name.clone(),
            directory: host.directories[self.directory_index].clone(),
            preset,
            command,
            placement,
        })
    }

    fn current_len(&self) -> usize {
        match self.stage {
            PickerStage::Host => self.options.hosts.len(),
            PickerStage::Resource => self.options.hosts[self.host_index].workloads.len() + 1,
            PickerStage::Directory => self.options.hosts[self.host_index].directories.len(),
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
            PickerStage::Resource => "Workloads",
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
                    .chain(std::iter::once("Create new workload"))
                    .collect()
            }
            PickerStage::Directory => self.options.hosts[self.host_index]
                .directories
                .iter()
                .map(String::as_str)
                .collect(),
            PickerStage::Command => self.options.hosts[self.host_index]
                .commands
                .iter()
                .map(PickerCommand::label)
                .collect(),
            PickerStage::Placement => PLACEMENT_LABELS.to_vec(),
        }
    }
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

/// Runs the interactive terminal explorer. Escape and Ctrl-C always return
/// `Ok(None)`; terminal modes are restored before the function returns.
pub fn run_picker(
    options: PickerOptions,
    status_service: StatusService,
) -> Result<Option<PickerSelection>> {
    let mut state = PickerState::new(options)?;
    enable_raw_mode().context("enable terminal raw mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("enter terminal alternate screen");
    }

    let result = run_terminal_picker(&mut state, &status_service);
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

fn run_terminal_picker(
    state: &mut PickerState,
    status_service: &StatusService,
) -> Result<Option<PickerSelection>> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize terminal picker")?;
    terminal.clear().context("clear terminal picker")?;
    let mut generation = 1_u64;
    let mut status_run = start_status_run(state, status_service, generation);
    let mut dirty = true;

    loop {
        while let Ok(message) = status_run.receiver.try_recv() {
            dirty |= state.apply_status(message);
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
        let Some(picker_event) = map_key(key) else {
            continue;
        };
        dirty = true;
        match state.handle(picker_event) {
            PickerOutcome::Continue => {}
            PickerOutcome::RefreshRequested => {
                status_run.cancel();
                generation = generation
                    .checked_add(1)
                    .context("status refresh generation overflow")?;
                status_run = start_status_run(state, status_service, generation);
            }
            PickerOutcome::Selected(selection) => return Ok(Some(selection)),
            PickerOutcome::Cancelled => return Ok(None),
        }
    }
}

fn map_key(key: KeyEvent) -> Option<PickerEvent> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(PickerEvent::Cancel);
    }
    match key.code {
        KeyCode::Esc => Some(PickerEvent::Cancel),
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Some(PickerEvent::Previous),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Some(PickerEvent::Next),
        KeyCode::Backspace | KeyCode::Left => Some(PickerEvent::Back),
        KeyCode::Enter | KeyCode::Right => Some(PickerEvent::Confirm),
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
    let area = centered_rect(frame.area(), 72, visible_rows.saturating_add(6).max(8));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" Tether · {} ", state.title()))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(block, area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let selected = state.current_index();
    let max_rows = chunks[0].height as usize;
    let start = selected.saturating_sub(max_rows.saturating_sub(1));
    let lines: Vec<Line<'_>> = labels
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .map(|(index, label)| {
            if index == selected {
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
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
        Paragraph::new("↑/↓ navigate · Enter/→ select · r refresh · ← back · Esc cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
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
            map_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(PickerEvent::Refresh)
        );
        assert_eq!(
            map_key(KeyEvent::new_with_kind(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
        assert_eq!(
            map_key(KeyEvent::new_with_kind(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            None
        );
    }
}
