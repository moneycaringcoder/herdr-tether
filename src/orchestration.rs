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
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    backend::ProcessBinaries,
    config::ConfigStore,
    herdr::{HerdrClient, HerdrContext, PaneTitle},
    lifecycle::LifecycleService,
    model::{OrchestrationGroupId, OrchestrationTitle, Placement, SessionId},
    observer::{
        ObserverCapabilities, ObserverKey, ObserverLifecycle, ObserverOutcome, ObserverState,
        ObserverWorker, action_for_key, render,
    },
    paths::AppPaths,
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, StateStore,
    },
    tmux::TmuxBackend,
};

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
            let group = state
                .orchestration_groups
                .iter_mut()
                .find(|group| &group.id == group_id)
                .with_context(|| format!("unknown orchestration group `{group_id}`"))?;
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
                title,
                capabilities,
            };
            group.workers.push(worker.clone());
            Ok(worker)
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

pub fn run_observer(
    paths: &AppPaths,
    group_id: OrchestrationGroupId,
    herdr_context: HerdrContext,
) -> Result<()> {
    let store = StateStore::new(paths.state_file.clone());
    let service = OrchestrationService::new(store.clone());
    let group = service.group(&group_id)?;
    let mut observer = ObserverState::new(Vec::with_capacity(group.workers.len()));
    let (capture_worker, observer_results) = CaptureWorker::spawn();
    refresh_observer_metadata(&store, &group_id, &mut observer, capture_worker.sender())?;

    enable_raw_mode().context("enable Observer terminal raw mode")?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("enter Observer alternate screen");
    }
    let _guard = ObserverTerminalGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize Observer terminal")?;
    terminal.clear().context("clear Observer terminal")?;
    let mut last_refresh = Instant::now();

    loop {
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(frame, area, &observer);
            })
            .context("draw Observer")?;

        loop {
            match observer_results.try_recv() {
                Ok((visible, workers)) if visible_worker_ids(&observer) == visible => {
                    merge_captured_workers(&mut observer, &visible, workers);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    bail!("Observer capture worker stopped unexpectedly")
                }
            }
        }
        if last_refresh.elapsed() >= OBSERVER_REFRESH_INTERVAL {
            refresh_observer_metadata(&store, &group_id, &mut observer, capture_worker.sender())?;
            last_refresh = Instant::now();
        }
        if !event::poll(OBSERVER_INPUT_POLL).context("poll Observer input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read Observer input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let observer_key = if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            ObserverKey::ControlC
        } else {
            match key.code {
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
                _ => continue,
            }
        };
        let Some(action) = action_for_key(observer_key) else {
            continue;
        };
        observer.set_notice(None);
        let previous_page = observer.page();
        match observer.apply(action) {
            ObserverOutcome::None if observer.page() != previous_page => {
                refresh_observer_metadata(
                    &store,
                    &group_id,
                    &mut observer,
                    capture_worker.sender(),
                )?;
                last_refresh = Instant::now();
            }
            ObserverOutcome::None => {}
            ObserverOutcome::Refresh => {
                refresh_observer_metadata(
                    &store,
                    &group_id,
                    &mut observer,
                    capture_worker.sender(),
                )?;
                last_refresh = Instant::now();
            }
            ObserverOutcome::OpenSelected { worker_id } => {
                if let Err(error) =
                    open_worker(paths, &store, &group_id, &worker_id, &herdr_context)
                {
                    observer.set_notice(Some(format!("Open failed: {error:#}")));
                }
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

fn refresh_observer_metadata(
    store: &StateStore,
    group_id: &OrchestrationGroupId,
    observer: &mut ObserverState,
    capture_requests: &SyncSender<CaptureRequest>,
) -> Result<()> {
    let state = store.load()?;
    let group = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
        .with_context(|| format!("orchestration group `{group_id}` was deleted"))?;
    update_observer_metadata(
        observer,
        observer_workers(group, &state.sessions, &HashSet::new()),
    );
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

type CaptureResult = (HashSet<String>, Vec<ObserverWorker>);

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
                let workers = observer_workers(&request.group, &request.sessions, &request.visible);
                if results.send((request.visible, workers)).is_err() {
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

fn update_observer_metadata(observer: &mut ObserverState, mut workers: Vec<ObserverWorker>) {
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
        if worker.lifecycle == ObserverLifecycle::Running
            && worker.capabilities.observe_output
            && let Some(capture) = previous_captures.get(&worker.id)
        {
            worker.capture = Some(capture.clone());
        }
    }
    observer.update_workers(workers);
}

fn merge_captured_workers(
    observer: &mut ObserverState,
    visible: &HashSet<String>,
    captured: Vec<ObserverWorker>,
) {
    let mut workers = observer.workers().to_vec();
    for worker in &mut workers {
        if !visible.contains(&worker.id) {
            continue;
        }
        let Some(result) = captured.iter().find(|candidate| {
            candidate.id == worker.id
                && candidate.title == worker.title
                && candidate.capabilities == worker.capabilities
                && candidate.lifecycle == worker.lifecycle
                && candidate.owned == worker.owned
        }) else {
            continue;
        };
        worker.capture.clone_from(&result.capture);
    }
    observer.update_workers(workers);
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

fn observer_workers(
    group: &OrchestrationGroup,
    sessions: &[SessionRecord],
    visible: &HashSet<String>,
) -> Vec<ObserverWorker> {
    let sessions_by_id: HashMap<_, _> = sessions.iter().map(|record| (record.id, record)).collect();
    let mut workers = group
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
        .collect::<Vec<_>>();

    let captures = thread::scope(|scope| {
        let handles = group
            .workers
            .iter()
            .enumerate()
            .filter_map(|(index, member)| {
                let id = member.session_id.to_string();
                let record = sessions_by_id.get(&member.session_id).copied()?;
                (visible.contains(&id)
                    && member.capabilities.observe_output
                    && record.status == SessionStatus::Running)
                    .then(|| (index, scope.spawn(move || capture_record(record))))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(index, handle)| {
                (
                    index,
                    handle.join().unwrap_or_else(|_| {
                        Some("[capture unavailable: worker panicked]".to_owned())
                    }),
                )
            })
            .collect::<Vec<_>>()
    });
    for (index, capture) in captures {
        workers[index].capture = capture;
    }
    workers
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

struct ObserverTerminalGuard;

impl Drop for ObserverTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn metadata_refresh_preserves_only_still_authorized_running_captures() {
        let mut observer = ObserverState::new(vec![
            worker("running", ObserverLifecycle::Running, Some("previous")),
            worker("ended", ObserverLifecycle::Running, Some("stale")),
        ]);

        update_observer_metadata(
            &mut observer,
            vec![
                worker("new", ObserverLifecycle::Starting, None),
                worker("running", ObserverLifecycle::Running, None),
                worker("ended", ObserverLifecycle::Ended, None),
            ],
        );

        assert_eq!(observer.workers()[0].capture, None);
        assert_eq!(observer.workers()[1].capture.as_deref(), Some("previous"));
        assert_eq!(observer.workers()[2].capture, None);
    }

    #[test]
    fn capture_results_merge_by_identity_and_current_metadata_only() {
        let mut observer = ObserverState::new(vec![
            worker("current", ObserverLifecycle::Running, Some("old")),
            worker("changed", ObserverLifecycle::Ended, None),
            worker("off-page", ObserverLifecycle::Running, Some("kept")),
        ]);
        let captured = vec![
            worker("current", ObserverLifecycle::Running, Some("new")),
            worker("changed", ObserverLifecycle::Running, Some("stale")),
            worker("removed", ObserverLifecycle::Running, Some("irrelevant")),
            worker("off-page", ObserverLifecycle::Running, None),
        ];

        let visible = HashSet::from(["current".to_owned(), "changed".to_owned()]);
        merge_captured_workers(&mut observer, &visible, captured);

        assert_eq!(observer.workers()[0].capture.as_deref(), Some("new"));
        assert_eq!(observer.workers()[1].capture, None);
        assert_eq!(observer.workers()[2].capture.as_deref(), Some("kept"));
        assert_eq!(observer.selected_id(), Some("current"));
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
}
