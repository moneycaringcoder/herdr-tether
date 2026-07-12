//! Pure projection and rendering for the read-only orchestration observer.
//!
//! This module deliberately knows nothing about sessions, tmux, or any orchestration
//! harness. Runtime integrations translate their state into [`ObserverWorker`] values
//! and translate [`ObserverOutcome`] values back into application actions.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
};

use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

pub const WORKERS_PER_PAGE: usize = 4;
pub const MAX_WORKERS: usize = 64;
pub const MAX_CAPTURE_LINES: usize = 200;
pub const MAX_CAPTURE_BYTES: usize = 16 * 1024;
pub const MAX_CAPTURE_CELLS: usize = 16 * 1024;

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

    fn display_title(&self) -> String {
        let title = self
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or(&self.id);
        sanitize_label(title, 160)
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObserverState {
    workers: Vec<ObserverWorker>,
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

    /// Replaces membership while preserving selection by worker identity.
    ///
    /// Duplicate IDs after their first occurrence and workers beyond [`MAX_WORKERS`]
    /// are ignored. If the selected identity disappeared, the prior numeric position
    /// is retained where possible.
    pub fn update_workers(&mut self, workers: Vec<ObserverWorker>) {
        let previous_index = self.selected_index().unwrap_or(0);
        let mut seen = HashSet::with_capacity(workers.len().min(MAX_WORKERS));
        self.workers = workers
            .into_iter()
            .filter(|worker| seen.insert(worker.id.clone()))
            .take(MAX_WORKERS)
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

pub fn render(frame: &mut Frame<'_>, area: Rect, observer: &ObserverState) {
    if area.is_empty() {
        return;
    }

    if area.height < 3 {
        let compact = format!("Observer {}/{}", observer.page() + 1, observer.page_count());
        frame.render_widget(Paragraph::new(compact), area);
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let canvas = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    let worker_count = observer.workers.len();
    let noun = if worker_count == 1 {
        "worker"
    } else {
        "workers"
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Observer  {worker_count} {noun}  page {}/{}",
            observer.page() + 1,
            observer.page_count()
        )),
        header,
    );

    let visible = observer.visible_workers();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new("No workers registered"), canvas);
    } else {
        for (worker, rect) in visible.iter().zip(worker_rects(canvas, visible.len())) {
            render_worker(
                frame,
                rect,
                worker,
                observer.selected_id() == Some(worker.id.as_str()),
            );
        }
    }

    let hidden = worker_count.saturating_sub((observer.page() + 1) * WORKERS_PER_PAGE);
    let overflow = if hidden == 0 {
        String::new()
    } else {
        format!("+{hidden} more  ")
    };
    let controls = format!("{overflow}↑↓ select  Tab/[ ] page  r refresh  Enter open  q quit");
    let footer_text = observer.notice().map_or(controls, |notice| {
        format!(
            "! {}  · q quit",
            sanitize_capture(notice).replace('\n', " ")
        )
    });
    frame.render_widget(Paragraph::new(footer_text), footer);
}

fn render_worker(frame: &mut Frame<'_>, area: Rect, worker: &ObserverWorker, selected: bool) {
    if area.is_empty() {
        return;
    }
    let marker = if selected { "▶ " } else { "  " };
    let eligibility = if worker.can_open() { " · OPEN" } else { "" };
    let title = format!(
        "{marker}{} · {}{eligibility}",
        worker.display_title(),
        worker.lifecycle.label()
    );
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title))
        .style(style);
    let body = if !worker.capabilities.observe_output {
        "Output not authorized".to_owned()
    } else {
        worker
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
            .unwrap_or_else(|| "No captured output".to_owned())
    };
    frame.render_widget(Paragraph::new(body).block(block), area);
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
    for (index, character) in input.char_indices().rev() {
        let character_bytes = character.len_utf8();
        if character == '\n' {
            if lines >= MAX_CAPTURE_LINES || bytes + character_bytes > MAX_CAPTURE_BYTES {
                break;
            }
            lines += 1;
        } else {
            let width = display_width(character);
            if bytes + character_bytes > MAX_CAPTURE_BYTES || cells + width > MAX_CAPTURE_CELLS {
                break;
            }
            cells += width;
        }
        bytes += character_bytes;
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
        for character in line.chars() {
            let character_width = display_width(character);
            if cells > 0 && cells + character_width > width {
                push_viewport_row(&mut rows, std::mem::take(&mut row), height);
                cells = 0;
            }
            if character_width > width {
                continue;
            }
            row.push(character);
            cells += character_width;
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
    for character in capture.chars() {
        let character = if character == '\n' { ' ' } else { character };
        let width = display_width(character);
        if cells + width > max_cells {
            break;
        }
        output.push(character);
        cells += width;
    }
    output
}

fn is_unsafe_format(character: char) -> bool {
    matches!(
        character,
        '\u{0300}'..='\u{036f}'
            | '\u{0483}'..='\u{0489}'
            | '\u{0610}'..='\u{061a}'
            | '\u{061c}'
            | '\u{064b}'..='\u{065f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    ) || ('\u{fe00}'..='\u{fe0f}').contains(&character)
        || ('\u{e0100}'..='\u{e01ef}').contains(&character)
}

fn display_width(character: char) -> usize {
    if character == '\0' || is_unsafe_format(character) || character.is_control() {
        0
    } else if matches!(
        character,
        '\u{1100}'..='\u{115f}'
            | '\u{2329}'..='\u{232a}'
            | '\u{2e80}'..='\u{a4cf}'
            | '\u{ac00}'..='\u{d7a3}'
            | '\u{f900}'..='\u{faff}'
            | '\u{fe10}'..='\u{fe19}'
            | '\u{fe30}'..='\u{fe6f}'
            | '\u{ff00}'..='\u{ff60}'
            | '\u{ffe0}'..='\u{ffe6}'
            | '\u{1f300}'..='\u{1faff}'
            | '\u{20000}'..='\u{3fffd}'
    ) {
        2
    } else {
        1
    }
}
