use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::process::CommandExt};

use crate::{
    backend::{CommandSpec, ProcessBinaries},
    model::{ExternalSessionName, SessionId},
    tmux::TmuxBackend,
};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_DRAIN_BYTES_PER_TICK: usize = MAX_CAPTURE_BYTES + 8192;
const MAX_PROCESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_STATUS_WORKERS: usize = 16;
pub const MAX_STATUS_HOSTS: usize = 1_024;
pub const MAX_STATUS_WORKLOADS: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostReachability {
    Reachable,
    Unreachable,
    TimedOut,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadStatus {
    Running { attached: u32 },
    Missing,
    Unknown,
    TimedOut,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalCatalogStatus {
    Available,
    Unavailable,
    TimedOut,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalSession {
    pub name: ExternalSessionName,
    pub attached: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusHost {
    pub name: String,
    /// `None` means the local host; `Some` is a validated OpenSSH target.
    pub target: Option<String>,
    pub workloads: Vec<SessionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusRequest {
    pub generation: u64,
    pub hosts: Vec<StatusHost>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusRequestError {
    TooManyHosts { actual: usize, maximum: usize },
    TooManyWorkloads { actual: usize, maximum: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusMessage {
    Host {
        generation: u64,
        host: String,
        status: HostReachability,
        detail: Option<String>,
        checked_at: SystemTime,
    },
    Workload {
        generation: u64,
        id: SessionId,
        status: WorkloadStatus,
        checked_at: SystemTime,
    },
    Catalog {
        generation: u64,
        host: String,
        status: ExternalCatalogStatus,
        sessions: Vec<ExternalSession>,
        hidden_reserved: usize,
        hidden_unsafe: usize,
        checked_at: SystemTime,
    },
    Finished {
        generation: u64,
    },
}

impl StatusMessage {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Host { generation, .. }
            | Self::Workload { generation, .. }
            | Self::Catalog { generation, .. }
            | Self::Finished { generation } => *generation,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StatusService {
    binaries: ProcessBinaries,
    timeout: Duration,
    workers: usize,
}

impl StatusService {
    pub fn new(binaries: ProcessBinaries, timeout: Duration, workers: usize) -> Self {
        Self {
            binaries,
            timeout,
            workers: workers.clamp(1, MAX_STATUS_WORKERS),
        }
    }

    pub fn start(&self, request: StatusRequest) -> StatusRun {
        let generation = request.generation;
        self.try_start(request)
            .unwrap_or_else(|_| finished_run(generation))
    }

    pub fn try_start(&self, mut request: StatusRequest) -> Result<StatusRun, StatusRequestError> {
        validate_status_request(&request)?;
        request.hosts = normalize_status_hosts(request.hosts);
        Ok(self.start_validated(request))
    }

    fn start_validated(&self, request: StatusRequest) -> StatusRun {
        let (sender, receiver) =
            mpsc::sync_channel(MAX_STATUS_WORKLOADS + MAX_STATUS_HOSTS * 2 + 1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let jobs = Arc::new(Mutex::new(VecDeque::from(request.hosts)));
        let worker_count = self
            .workers
            .min(jobs.lock().expect("status jobs lock").len().max(1));
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let sender = sender.clone();
            let cancelled = Arc::clone(&cancelled);
            let binaries = self.binaries.clone();
            let timeout = self.timeout;
            let generation = request.generation;
            handles.push(thread::spawn(move || {
                worker_loop(generation, jobs, &sender, &cancelled, &binaries, timeout);
            }));
        }

        thread::spawn(move || {
            for handle in handles {
                let _ = handle.join();
            }
            let _ = sender.send(StatusMessage::Finished {
                generation: request.generation,
            });
        });

        StatusRun {
            receiver,
            cancelled,
        }
    }
}

fn validate_status_request(request: &StatusRequest) -> Result<(), StatusRequestError> {
    if request.hosts.len() > MAX_STATUS_HOSTS {
        return Err(StatusRequestError::TooManyHosts {
            actual: request.hosts.len(),
            maximum: MAX_STATUS_HOSTS,
        });
    }
    let workload_count = request
        .hosts
        .iter()
        .try_fold(0usize, |total, host| {
            total.checked_add(host.workloads.len())
        })
        .unwrap_or(usize::MAX);
    if workload_count > MAX_STATUS_WORKLOADS {
        return Err(StatusRequestError::TooManyWorkloads {
            actual: workload_count,
            maximum: MAX_STATUS_WORKLOADS,
        });
    }
    Ok(())
}

fn normalize_status_hosts(hosts: Vec<StatusHost>) -> Vec<StatusHost> {
    let mut normalized = Vec::<StatusHost>::new();
    let mut host_indexes = HashMap::<(String, Option<String>), usize>::new();
    let mut workload_sets = Vec::<HashSet<SessionId>>::new();
    for host in hosts {
        let key = (host.name.clone(), host.target.clone());
        let index = match host_indexes.get(&key).copied() {
            Some(index) => index,
            None => {
                let index = normalized.len();
                host_indexes.insert(key, index);
                normalized.push(StatusHost {
                    name: host.name,
                    target: host.target,
                    workloads: Vec::new(),
                });
                workload_sets.push(HashSet::new());
                index
            }
        };
        for workload in host.workloads {
            if workload_sets[index].insert(workload) {
                normalized[index].workloads.push(workload);
            }
        }
    }
    normalized
}

fn finished_run(generation: u64) -> StatusRun {
    let (sender, receiver) = mpsc::sync_channel(1);
    let _ = sender.send(StatusMessage::Finished { generation });
    StatusRun {
        receiver,
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

#[derive(Debug)]

pub struct StatusRun {
    pub receiver: Receiver<StatusMessage>,
    cancelled: Arc<AtomicBool>,
}

impl StatusRun {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for StatusRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn worker_loop(
    generation: u64,
    jobs: Arc<Mutex<VecDeque<StatusHost>>>,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    timeout: Duration,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(host) = jobs.lock().expect("status jobs lock").pop_front() else {
            return;
        };
        probe_host(generation, host, sender, cancelled, binaries, timeout);
    }
}

fn probe_host(
    generation: u64,
    host: StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    timeout: Duration,
) {
    let backend = match &host.target {
        Some(target) => TmuxBackend::remote(target.clone(), binaries.clone()),
        None => Ok(TmuxBackend::local(binaries.clone())),
    };
    let result = backend
        .and_then(|backend| backend.status_spec())
        .map_or(BoundedOutput::Error, |spec| {
            run_bounded(&spec, timeout, cancelled)
        });
    if matches!(result, BoundedOutput::Cancelled) || cancelled.load(Ordering::Acquire) {
        return;
    }

    let checked_at = SystemTime::now();
    let classified = classify_result(&host, result);
    if sender
        .send(StatusMessage::Host {
            generation,
            host: host.name.clone(),
            status: classified.reachability,
            detail: classified.detail,
            checked_at,
        })
        .is_err()
    {
        return;
    }
    if cancelled.load(Ordering::Acquire)
        || sender
            .send(StatusMessage::Catalog {
                generation,
                host: host.name,
                status: classified.catalog_status,
                sessions: classified.external,
                hidden_reserved: classified.hidden_reserved,
                hidden_unsafe: classified.hidden_unsafe,
                checked_at,
            })
            .is_err()
    {
        return;
    }
    for (id, status) in classified.workloads {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if sender
            .send(StatusMessage::Workload {
                generation,
                id,
                status,
                checked_at,
            })
            .is_err()
        {
            return;
        }
    }
}

struct ClassifiedResult {
    reachability: HostReachability,
    detail: Option<String>,
    workloads: Vec<(SessionId, WorkloadStatus)>,
    catalog_status: ExternalCatalogStatus,
    external: Vec<ExternalSession>,
    hidden_reserved: usize,
    hidden_unsafe: usize,
}

fn classify_result(host: &StatusHost, result: BoundedOutput) -> ClassifiedResult {
    let requested = host.workloads.iter().copied().collect::<HashSet<_>>();
    match result {
        BoundedOutput::Completed {
            status,
            stdout,
            stdout_truncated: false,
            ..
        } if status.success() => match parse_sessions(&stdout) {
            Some(catalog) => ClassifiedResult {
                reachability: HostReachability::Reachable,
                detail: None,
                workloads: host
                    .workloads
                    .iter()
                    .map(|id| {
                        let status =
                            catalog
                                .owned
                                .get(id)
                                .map_or(WorkloadStatus::Missing, |attached| {
                                    WorkloadStatus::Running {
                                        attached: *attached,
                                    }
                                });
                        (*id, status)
                    })
                    .collect(),
                catalog_status: ExternalCatalogStatus::Available,
                external: catalog.external,
                hidden_reserved: catalog.hidden_reserved
                    + catalog
                        .owned
                        .keys()
                        .filter(|id| !requested.contains(id))
                        .count(),
                hidden_unsafe: catalog.unsafe_names,
            },
            None => with_detail(
                classified_failure(
                    host,
                    HostReachability::Reachable,
                    WorkloadStatus::Unknown,
                    ExternalCatalogStatus::Error,
                ),
                "tmux returned an invalid status response".to_owned(),
            ),
        },
        BoundedOutput::Completed { status, .. } if status.success() => with_detail(
            classified_failure(
                host,
                HostReachability::Reachable,
                WorkloadStatus::Unknown,
                ExternalCatalogStatus::Error,
            ),
            "tmux status response exceeded the safe capture limit".to_owned(),
        ),
        BoundedOutput::Completed { status, .. } if status.code() == Some(1) => ClassifiedResult {
            reachability: HostReachability::Reachable,
            detail: None,
            workloads: uniform_workloads(&host.workloads, WorkloadStatus::Missing),
            catalog_status: ExternalCatalogStatus::Available,
            external: Vec::new(),
            hidden_reserved: 0,
            hidden_unsafe: 0,
        },
        BoundedOutput::Completed { status, stderr, .. }
            if host.target.is_some() && status.code() == Some(255) =>
        {
            with_detail(
                classified_failure(
                    host,
                    HostReachability::Unreachable,
                    WorkloadStatus::Unknown,
                    ExternalCatalogStatus::Unavailable,
                ),
                classify_ssh_failure(&stderr).to_owned(),
            )
        }
        BoundedOutput::TimedOut => with_detail(
            classified_failure(
                host,
                HostReachability::TimedOut,
                WorkloadStatus::TimedOut,
                ExternalCatalogStatus::TimedOut,
            ),
            "status probe timed out; retry after checking the host connection".to_owned(),
        ),
        BoundedOutput::SpawnError(kind) => {
            let program = if host.target.is_some() { "ssh" } else { "tmux" };
            with_detail(
                classified_failure(
                    host,
                    HostReachability::Error,
                    WorkloadStatus::Error,
                    ExternalCatalogStatus::Error,
                ),
                format!(
                    "could not start {program} ({kind:?}); install it or make it executable in PATH, /opt/homebrew/bin, or /usr/local/bin"
                ),
            )
        }
        BoundedOutput::Error => with_detail(
            classified_failure(
                host,
                HostReachability::Error,
                WorkloadStatus::Error,
                ExternalCatalogStatus::Error,
            ),
            "status probe failed while reading process output; retry or run `herdr-tether doctor`"
                .to_owned(),
        ),
        BoundedOutput::Completed {
            status,
            stderr,
            stderr_truncated,
            ..
        } => {
            let detail = if host.target.is_some() {
                format!(
                    "remote status probe exited with {status}; retry after checking SSH and tmux"
                )
            } else {
                let mut detail = sanitize_process_detail(&stderr);
                if detail.is_empty() {
                    detail = format!("status probe exited with {status}");
                } else if stderr_truncated {
                    detail.push('…');
                }
                detail
            };
            with_detail(
                classified_failure(
                    host,
                    HostReachability::Error,
                    WorkloadStatus::Error,
                    ExternalCatalogStatus::Error,
                ),
                detail,
            )
        }
        BoundedOutput::Cancelled => unreachable!("cancelled probes do not publish"),
    }
}

fn classify_ssh_failure(stderr: &[u8]) -> &'static str {
    let lower = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if lower.contains("connection refused") {
        "SSH connection was refused; check that the host is online and SSH is running"
    } else if lower.contains("permission denied") {
        "SSH authentication failed; check the configured user and credentials"
    } else if lower.contains("host key verification failed") {
        "SSH host verification failed; verify the host key before retrying"
    } else if lower.contains("could not resolve hostname")
        || lower.contains("name or service not known")
    {
        "SSH could not resolve the host name; check the configured target"
    } else if lower.contains("timed out") || lower.contains("no route to host") {
        "SSH could not reach the host; check its network connection"
    } else {
        "SSH connection failed; retry after checking the host and credentials"
    }
}

fn sanitize_process_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut pending_space = false;
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if chars.next_if_eq(&'[').is_some() {
                for sequence in chars.by_ref() {
                    if ('@'..='~').contains(&sequence) {
                        break;
                    }
                }
            }
            continue;
        }
        if character.is_whitespace() || character.is_control() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(character);
    }
    cleaned
}

fn classified_failure(
    host: &StatusHost,
    reachability: HostReachability,
    workload_status: WorkloadStatus,
    catalog_status: ExternalCatalogStatus,
) -> ClassifiedResult {
    ClassifiedResult {
        detail: None,
        reachability,
        workloads: uniform_workloads(&host.workloads, workload_status),
        catalog_status,
        external: Vec::new(),
        hidden_reserved: 0,
        hidden_unsafe: 0,
    }
}

fn with_detail(mut classified: ClassifiedResult, detail: String) -> ClassifiedResult {
    classified.detail = Some(detail);
    classified
}

fn uniform_workloads(
    workloads: &[SessionId],
    status: WorkloadStatus,
) -> Vec<(SessionId, WorkloadStatus)> {
    workloads.iter().map(|id| (*id, status)).collect()
}

struct ParsedSessions {
    owned: HashMap<SessionId, u32>,
    external: Vec<ExternalSession>,
    hidden_reserved: usize,
    unsafe_names: usize,
}

fn parse_sessions(stdout: &[u8]) -> Option<ParsedSessions> {
    const MAX_SESSIONS: usize = 256;

    let text = std::str::from_utf8(stdout).ok()?;
    let mut names = HashSet::new();
    let mut owned = HashMap::new();
    let mut external = Vec::new();
    let mut hidden_reserved = 0;
    let mut unsafe_names = 0;
    for line in text.lines().filter(|line| !line.is_empty()) {
        if names.len() >= MAX_SESSIONS {
            return None;
        }
        let (name, attached) = line.rsplit_once(':')?;
        if attached.contains(':') || !names.insert(name.to_owned()) {
            return None;
        }
        let attached = attached.parse::<u32>().ok()?;
        if name.starts_with("tether-") {
            if let Ok(id) = name.parse::<SessionId>() {
                owned.insert(id, attached);
            } else {
                hidden_reserved += 1;
            }
            continue;
        }
        match name.parse() {
            Ok(name) => external.push(ExternalSession { name, attached }),
            Err(_) => unsafe_names += 1,
        }
    }
    external.sort_by(|left, right| left.name.cmp(&right.name));
    Some(ParsedSessions {
        owned,
        external,
        hidden_reserved,
        unsafe_names,
    })
}

pub(crate) enum BoundedOutput {
    Completed {
        status: ExitStatus,
        stdout: Vec<u8>,
        stdout_truncated: bool,
        stderr: Vec<u8>,
        stderr_truncated: bool,
    },
    TimedOut,
    Cancelled,
    SpawnError(io::ErrorKind),
    Error,
}

pub(crate) fn run_bounded(
    spec: &CommandSpec,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> BoundedOutput {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return BoundedOutput::SpawnError(error.kind()),
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        terminate_child(&mut child);
        return BoundedOutput::Error;
    }
    let mut stdout_capture = Capture::default();
    let mut stderr_capture = Capture::default();
    let deadline = Instant::now() + timeout.min(MAX_PROCESS_TIMEOUT);

    loop {
        if drain_pipe(stdout.as_mut(), &mut stdout_capture).is_err()
            || drain_pipe(stderr.as_mut(), &mut stderr_capture).is_err()
        {
            terminate_child(&mut child);
            return BoundedOutput::Error;
        }
        if cancelled.load(Ordering::Acquire) {
            terminate_child(&mut child);
            let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
            let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
            return BoundedOutput::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // The direct child may have successfully forked descendants
                // into its fresh process group. End them before returning so a
                // completed bounded command cannot leave orphaned work behind.
                kill_process_group(child.id());
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
                return BoundedOutput::Completed {
                    status,
                    stdout: stdout_capture.bytes,
                    stdout_truncated: stdout_capture.truncated,
                    stderr: stderr_capture.bytes,
                    stderr_truncated: stderr_capture.truncated,
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
                return BoundedOutput::TimedOut;
            }
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            Err(_) => {
                terminate_child(&mut child);
                return BoundedOutput::Error;
            }
        }
    }
}

fn terminate_child(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn kill_process_group(id: u32) {
    // SAFETY: each active probe is spawned into a fresh process group whose id
    // is the direct child's pid. SIGKILL has no Rust memory-safety preconditions.
    unsafe {
        libc::killpg(id as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_id: u32) {}

#[cfg(unix)]
fn set_nonblocking<T: AsRawFd>(pipe: &Option<T>) -> io::Result<()> {
    let Some(pipe) = pipe else {
        return Ok(());
    };
    // SAFETY: fcntl is called with a live pipe fd and integer-only commands.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the same live fd is updated with its existing flags plus O_NONBLOCK.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking<T>(_pipe: &Option<T>) -> io::Result<()> {
    Ok(())
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain_pipe<R: Read>(pipe: Option<&mut R>, capture: &mut Capture) -> io::Result<()> {
    let Some(pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    let mut drained = 0;
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => {
                drained += length;
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(capture.bytes.len());
                let retained = remaining.min(length);
                capture.bytes.extend_from_slice(&buffer[..retained]);
                capture.truncated |= retained < length;
                if drained >= MAX_DRAIN_BYTES_PER_TICK {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_worker_count_has_an_internal_ceiling() {
        let service = StatusService::new(
            ProcessBinaries::new("ssh", "tmux"),
            Duration::from_secs(1),
            usize::MAX,
        );
        assert_eq!(service.workers, MAX_STATUS_WORKERS);
    }
}
