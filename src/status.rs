use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::process::CommandExt};

use crate::{
    backend::{CommandSpec, ProcessBinaries},
    model::SessionId,
    tmux::TmuxBackend,
};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_DRAIN_BYTES_PER_TICK: usize = MAX_CAPTURE_BYTES + 8192;

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
pub enum StatusMessage {
    Host {
        generation: u64,
        host: String,
        status: HostReachability,
        checked_at: SystemTime,
    },
    Workload {
        generation: u64,
        id: SessionId,
        status: WorkloadStatus,
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
            workers: workers.max(1),
        }
    }

    pub fn start(&self, request: StatusRequest) -> StatusRun {
        let (sender, receiver) = mpsc::channel();
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
    sender: &Sender<StatusMessage>,
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
    sender: &Sender<StatusMessage>,
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
        .map_or(ProbeResult::Error, |spec| {
            run_bounded(&spec, timeout, cancelled)
        });
    if matches!(result, ProbeResult::Cancelled) || cancelled.load(Ordering::Acquire) {
        return;
    }

    let checked_at = SystemTime::now();
    let (reachability, workloads) = classify_result(&host, result);
    if sender
        .send(StatusMessage::Host {
            generation,
            host: host.name,
            status: reachability,
            checked_at,
        })
        .is_err()
    {
        return;
    }
    for (id, status) in workloads {
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

fn classify_result(
    host: &StatusHost,
    result: ProbeResult,
) -> (HostReachability, Vec<(SessionId, WorkloadStatus)>) {
    match result {
        ProbeResult::Completed {
            status,
            stdout,
            stdout_truncated: false,
        } if status.success() => match parse_sessions(&stdout) {
            Some(sessions) => (
                HostReachability::Reachable,
                host.workloads
                    .iter()
                    .map(|id| {
                        let status = sessions
                            .get(id)
                            .map_or(WorkloadStatus::Missing, |attached| {
                                WorkloadStatus::Running {
                                    attached: *attached,
                                }
                            });
                        (*id, status)
                    })
                    .collect(),
            ),
            None => (
                HostReachability::Reachable,
                uniform_workloads(&host.workloads, WorkloadStatus::Unknown),
            ),
        },
        ProbeResult::Completed { status, .. } if status.success() => (
            HostReachability::Reachable,
            uniform_workloads(&host.workloads, WorkloadStatus::Unknown),
        ),
        ProbeResult::Completed { status, .. } if status.code() == Some(1) => (
            HostReachability::Reachable,
            uniform_workloads(&host.workloads, WorkloadStatus::Missing),
        ),
        ProbeResult::Completed { status, .. }
            if host.target.is_some() && status.code() == Some(255) =>
        {
            (
                HostReachability::Unreachable,
                uniform_workloads(&host.workloads, WorkloadStatus::Unknown),
            )
        }
        ProbeResult::TimedOut => (
            HostReachability::TimedOut,
            uniform_workloads(&host.workloads, WorkloadStatus::TimedOut),
        ),
        ProbeResult::Error | ProbeResult::Completed { .. } => (
            HostReachability::Error,
            uniform_workloads(&host.workloads, WorkloadStatus::Error),
        ),
        ProbeResult::Cancelled => unreachable!("cancelled probes do not publish"),
    }
}

fn uniform_workloads(
    workloads: &[SessionId],
    status: WorkloadStatus,
) -> Vec<(SessionId, WorkloadStatus)> {
    workloads.iter().map(|id| (*id, status)).collect()
}

fn parse_sessions(stdout: &[u8]) -> Option<HashMap<SessionId, u32>> {
    let text = std::str::from_utf8(stdout).ok()?;
    let mut sessions = HashMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (name, attached) = line.split_once('\t')?;
        let Ok(id) = name.parse::<SessionId>() else {
            if name.starts_with("tether-") {
                return None;
            }
            continue;
        };
        let attached = attached.parse::<u32>().ok()?;
        if sessions.insert(id, attached).is_some() {
            return None;
        }
    }
    Some(sessions)
}

enum ProbeResult {
    Completed {
        status: ExitStatus,
        stdout: Vec<u8>,
        stdout_truncated: bool,
    },
    TimedOut,
    Cancelled,
    Error,
}

fn run_bounded(spec: &CommandSpec, timeout: Duration, cancelled: &AtomicBool) -> ProbeResult {
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
        Err(_) => return ProbeResult::Error,
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        terminate_child(&mut child);
        return ProbeResult::Error;
    }
    let mut stdout_capture = Capture::default();
    let mut stderr_capture = Capture::default();
    let deadline = Instant::now() + timeout;

    loop {
        if drain_pipe(stdout.as_mut(), &mut stdout_capture).is_err()
            || drain_pipe(stderr.as_mut(), &mut stderr_capture).is_err()
        {
            terminate_child(&mut child);
            return ProbeResult::Error;
        }
        if cancelled.load(Ordering::Acquire) {
            terminate_child(&mut child);
            let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
            let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
            return ProbeResult::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
                return ProbeResult::Completed {
                    status,
                    stdout: stdout_capture.bytes,
                    stdout_truncated: stdout_capture.truncated,
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture);
                return ProbeResult::TimedOut;
            }
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            Err(_) => {
                terminate_child(&mut child);
                return ProbeResult::Error;
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
