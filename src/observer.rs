//! Pure projection and rendering for the read-only orchestration observer.
//!
//! This module deliberately knows nothing about sessions, tmux, or any orchestration
//! harness. Runtime integrations translate their state into [`ObserverWorker`] values
//! and translate [`ObserverOutcome`] values back into application actions.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
};

use crate::model::SessionId;

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
const MIN_TILE_WIDTH: u16 = 12;
const MIN_TILE_HEIGHT: u16 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObserverCapabilities {
    pub observe_output: bool,
    pub open_interactive: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObserverLifecycle {
    Starting,
    Running,
    Stopping,
    Ended,
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
            Self::Missing => "MISSING",
            Self::Removed => "REMOVED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverCapture {
    Loading,
    Ready(String),
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureStatus {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverWorker {
    pub id: String,
    pub title: Option<String>,
    pub capabilities: ObserverCapabilities,
    pub lifecycle: ObserverLifecycle,
    pub owned: bool,
    /// Untrusted capture content. Rendering sanitizes and bounds it before display.
    pub capture: Option<String>,
}

impl ObserverWorker {
    pub fn can_open(&self) -> bool {
        self.owned
            && self.lifecycle == ObserverLifecycle::Running
            && self.capabilities.open_interactive
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
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserverOutcome {
    None,
    Refresh,
    OpenSelected { worker_id: String },
    OpenUnavailable { worker_id: String },
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
        ObserverAction::Refresh | ObserverAction::OpenSelected
            if kind == ObserverInputKind::Press && !busy =>
        {
            Some(action)
        }
        ObserverAction::Quit if kind == ObserverInputKind::Press => Some(action),
        ObserverAction::Refresh | ObserverAction::OpenSelected | ObserverAction::Quit => None,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObserverState {
    workers: Vec<ObserverWorker>,
    capture_statuses: HashMap<String, CaptureStatus>,
    selected_id: Option<String>,
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
        Some(action)
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
    pub fn update_workers(&mut self, workers: Vec<ObserverWorker>) {
        let previous_index = self.selected_index().unwrap_or(0);
        let previous_workers: HashMap<String, Option<String>> = self
            .workers
            .drain(..)
            .map(|worker| (worker.id, worker.capture))
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
                    if status == CaptureStatus::Ready {
                        worker.capture = previous_workers.get(&worker.id).cloned().flatten();
                    }
                    status
                };
                self.capture_statuses.insert(worker.id.clone(), status);
                worker
            })
            .collect();

        if self.workers.is_empty() {
            self.selected_id = None;
            return;
        }
        if let Some(selected) = self.selected_id.as_deref()
            && self.workers.iter().any(|worker| worker.id == selected)
        {
            return;
        }
        let index = previous_index.min(self.workers.len() - 1);
        self.selected_id = Some(self.workers[index].id.clone());
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
                CaptureStatus::Loading
            }
            ObserverCapture::Ready(capture) => {
                worker.capture = Some(sanitize_capture(&capture));
                CaptureStatus::Ready
            }
            ObserverCapture::Unavailable => {
                worker.capture = None;
                CaptureStatus::Unavailable
            }
        };
        self.capture_statuses.insert(worker_id.to_owned(), status);
        true
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
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
            Paragraph::new("Observer\nResize pane").style(observer_theme_style(false)),
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
        "Observer  0 workers".to_owned()
    } else {
        format!(
            "Observer  {worker_count} {noun}  page {}/{}",
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
                worker,
                observer
                    .capture_statuses
                    .get(&worker.id)
                    .copied()
                    .unwrap_or(CaptureStatus::Loading),
                &observer.worker_display_title(worker),
                observer.selected_id() == Some(worker.id.as_str()),
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
    } else if controls_height == 1 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("+{hidden} more  ")
        };
        format!("{overflow}↑↓ select  Tab/[ ] page  r refresh  Enter open  q back")
    } else if controls_height == 2 {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!("↑↓ select  [] page  r refresh\nEnter open  q back{overflow}")
    } else {
        let overflow = if hidden == 0 {
            String::new()
        } else {
            format!("  +{hidden} more")
        };
        format!("↑↓ select  [] page\nr refresh  Enter open\nq back{overflow}")
    };
    let controls_area = Rect::new(area.x, footer_y, area.width, controls_height);
    frame.render_widget(
        Paragraph::new(controls).style(observer_theme_style(false)),
        controls_area,
    );
}

fn render_worker(
    frame: &mut Frame<'_>,
    area: Rect,
    worker: &ObserverWorker,
    capture_status: CaptureStatus,
    display_title: &str,
    selected: bool,
) {
    if area.is_empty() {
        return;
    }
    let marker = if selected { "▶ " } else { "  " };
    let eligibility = if worker.can_open() { " · OPEN" } else { "" };
    let title = format!(
        "{marker}{} · {}{eligibility}",
        display_title,
        worker.lifecycle.label()
    );
    let style = observer_theme_style(selected);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title))
        .style(style);
    let body = if !worker.capabilities.observe_output {
        "Output not authorized".to_owned()
    } else {
        match capture_status {
            CaptureStatus::Loading => "Loading output".to_owned(),
            CaptureStatus::Unavailable => "Output unavailable".to_owned(),
            CaptureStatus::Ready => worker
                .capture
                .as_deref()
                .map(sanitize_capture)
                .filter(|capture| !capture.is_empty())
                .map(|capture| {
                    capture_viewport(
                        &capture,
                        area.width.saturating_sub(2),
                        area.height.saturating_sub(2),
                    )
                })
                .unwrap_or_else(|| "No captured output".to_owned()),
        }
    };
    frame.render_widget(
        Paragraph::new(body)
            .style(observer_theme_style(false))
            .block(block),
        area,
    );
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
