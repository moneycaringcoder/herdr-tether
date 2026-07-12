use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    backend::ProcessBinaries,
    config::ConfigStore,
    herdr::{HerdrClient, HerdrContext, PaneTitle},
    lifecycle::LifecycleService,
    model::{
        OrchestrationGroupId, OrchestrationMembershipId, OrchestrationTitle, OwnershipProof,
        Placement, SessionId, TmuxSessionId,
    },
    observer::{
        ObserverCapabilities, ObserverInputKind, ObserverKey, ObserverLifecycle, ObserverOutcome,
        ObserverState, ObserverWorker, render,
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

pub fn run_observer(
    paths: &AppPaths,
    group_id: OrchestrationGroupId,
    herdr_context: HerdrContext,
) -> Result<()> {
    let store = StateStore::new(paths.state_file.clone());
    let service = OrchestrationService::new(store.clone());
    let group = service.group(&group_id)?;
    let mut observer = ObserverState::new(Vec::with_capacity(group.workers.len()));
    let mut capture_fingerprints = HashMap::new();
    let (capture_worker, observer_results) = CaptureWorker::spawn();
    refresh_observer_metadata(
        &store,
        &group_id,
        &mut observer,
        &mut capture_fingerprints,
        capture_worker.sender(),
    )?;
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
                    merge_captured_workers(&store, &group_id, &mut observer, result)?;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    bail!("Observer capture worker stopped unexpectedly")
                }
            }
        }
        if last_refresh.elapsed() >= OBSERVER_REFRESH_INTERVAL {
            let refresh = refresh_observer_metadata(
                &store,
                &group_id,
                &mut observer,
                &mut capture_fingerprints,
                capture_worker.sender(),
            );
            apply_observer_refresh_result(&mut observer, refresh);
            last_refresh = Instant::now();
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
        // Actions finish synchronously before this loop polls again. ObserverOpenGate
        // separately suppresses queued/repeated opens, so no action is in flight here.
        let Some(action) = observer.action_for_input(observer_key, input_kind, false) else {
            continue;
        };
        let previous_page = observer.page();
        let outcome = observer.apply(action);
        if !matches!(&outcome, ObserverOutcome::OpenSelected { .. }) {
            observer.set_notice(None);
        }
        match outcome {
            ObserverOutcome::None if observer.page() != previous_page => {
                let refresh = refresh_observer_metadata(
                    &store,
                    &group_id,
                    &mut observer,
                    &mut capture_fingerprints,
                    capture_worker.sender(),
                );
                apply_observer_refresh_result(&mut observer, refresh);
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
                );
                apply_observer_refresh_result(&mut observer, refresh);
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
                    |worker_id| open_worker(paths, &store, &group_id, worker_id, &herdr_context),
                )?;
            }
            ObserverOutcome::OpenUnavailable { worker_id } => {
                observer.set_notice(Some(format!(
                    "Worker {worker_id} is not authorized, running, and exact-owned"
                )));
            }
            ObserverOutcome::Quit => return Ok(()),
        }
    }
}

fn apply_observer_refresh_result(observer: &mut ObserverState, result: Result<()>) -> bool {
    match result {
        Ok(()) => {
            observer.set_notice(None);
            true
        }
        Err(_) => {
            observer.set_notice(Some(
                "Refresh failed; stale output retained · r retry · q back".to_owned(),
            ));
            false
        }
    }
}

fn refresh_observer_metadata(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    observer: &mut ObserverState,
    capture_fingerprints: &mut HashMap<String, CaptureFingerprint>,
    capture_requests: &SyncSender<CaptureRequest>,
) -> Result<()> {
    let state = store.load()?;
    let group = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
        .with_context(|| format!("orchestration group `{group_id}` was deleted"))?;
    let next_fingerprints = capture_fingerprints_for(group, &state.sessions);
    update_observer_metadata(
        observer,
        capture_fingerprints,
        &next_fingerprints,
        observer_workers(group, &state.sessions),
    );
    *capture_fingerprints = next_fingerprints;
    let request = CaptureRequest {
        group: group.clone(),
        sessions: state.sessions,
        visible: visible_worker_ids(observer),
    };
    match capture_requests.try_send(request) {
        Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
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
    capture: Option<String>,
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
) {
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
        if previous_fingerprints.get(&worker.id) == current_fingerprints.get(&worker.id)
            && let Some(capture) = previous_captures.get(&worker.id)
        {
            worker.capture = Some(capture.clone());
        }
    }
    observer.update_workers(workers);
}

fn merge_captured_workers(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    observer: &mut ObserverState,
    result: CaptureResult,
) -> Result<()> {
    let state = store.load()?;
    let Some(group) = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
    else {
        return Ok(());
    };
    let current_fingerprints = capture_fingerprints_for(group, &state.sessions);
    let mut workers = observer.workers().to_vec();
    for captured in result.workers {
        let id = captured.fingerprint.session_id.to_string();
        if !result.visible.contains(&id)
            || current_fingerprints.get(&id) != Some(&captured.fingerprint)
        {
            continue;
        }
        if let Some(worker) = workers.iter_mut().find(|worker| worker.id == id) {
            worker.capture = captured.capture;
        }
    }
    observer.update_workers(workers);
    Ok(())
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
        .map(|worker| worker.id.clone())
        .collect()
}

fn observer_workers(group: &OrchestrationGroup, sessions: &[SessionRecord]) -> Vec<ObserverWorker> {
    let sessions_by_id: HashMap<_, _> = sessions.iter().map(|record| (record.id, record)).collect();
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
                Some(SessionStatus::Ended) => ObserverLifecycle::Ended,
                Some(SessionStatus::Removed) => ObserverLifecycle::Removed,
                None => ObserverLifecycle::Missing,
            };
            ObserverWorker {
                id: member.session_id.to_string(),
                title: member.title.as_ref().map(|title| title.as_str().to_owned()),
                capabilities: ObserverCapabilities {
                    observe_output: member.capabilities.observe_output,
                    open_interactive: member.capabilities.open_interactive,
                },
                lifecycle,
                owned,
                capture: None,
            }
        })
        .collect()
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
                (visible.contains(&id) && member.capabilities.observe_output)
                    .then(|| (fingerprint, scope.spawn(move || capture_record(record))))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(fingerprint, handle)| CapturedWorker {
                fingerprint,
                capture: handle
                    .join()
                    .unwrap_or_else(|_| Some("[capture unavailable: worker panicked]".to_owned())),
            })
            .collect()
    })
}

fn capture_record(record: &SessionRecord) -> Option<String> {
    let (Some(proof), Some(identity)) = (&record.ownership_proof, record.tmux_session_id) else {
        return None;
    };
    let backend = match backend_for_target(&record.target) {
        Ok(backend) => backend,
        Err(error) => return Some(format!("[capture unavailable: {error}]")),
    };
    Some(
        backend
            .capture_owned(&record.id, proof, identity)
            .map(|capture| capture.into_text())
            .unwrap_or_else(|error| format!("[capture unavailable: {error}]")),
    )
}

fn open_worker(
    paths: &AppPaths,
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    worker_id: &str,
    herdr_context: &HerdrContext,
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
    HerdrClient::new(herdr_context.clone()).place(&command, &title, placement)?;
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

    fn worker(id: &str, lifecycle: ObserverLifecycle, capture: Option<&str>) -> ObserverWorker {
        ObserverWorker {
            id: id.to_owned(),
            title: Some(format!("Worker {id}")),
            capabilities: ObserverCapabilities {
                observe_output: true,
                open_interactive: true,
            },
            lifecycle,
            owned: true,
            capture: capture.map(str::to_owned),
        }
    }

    fn exact_running_session(id: SessionId, tmux_id: &str) -> SessionRecord {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        SessionRecord {
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
            assert_eq!(
                apply_observer_refresh_result(&mut observer, result),
                expected_success
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

    fn fingerprint(membership: &str, identity: &str) -> CaptureFingerprint {
        CaptureFingerprint {
            membership_id: membership.parse().unwrap(),
            session_id: "tether-0197f198000070008000000000000002".parse().unwrap(),
            ownership_proof: "0197f198000070008000000000000099".parse().unwrap(),
            tmux_session_id: identity.parse().unwrap(),
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: true,
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
                        },
                    }],
                }],
            },
            group_id,
            worker_id,
        )
    }

    fn capture_result(fingerprint: CaptureFingerprint, capture: &str) -> CaptureResult {
        CaptureResult {
            visible: HashSet::from([fingerprint.session_id.to_string()]),
            workers: vec![CapturedWorker {
                fingerprint,
                capture: Some(capture.to_owned()),
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
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no longer a member"), "{error}");
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
            },
        };
        let second = OrchestrationMember {
            session_id: unaffected,
            membership_id: "0197f198000070008000000000000063".parse().unwrap(),
            title: Some("leave me".parse().unwrap()),
            capabilities: OrchestrationCapabilities {
                observe_output: false,
                open_interactive: true,
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
