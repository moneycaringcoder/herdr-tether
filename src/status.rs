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
    interrupt::{self, Budget},
    model::{ExternalSessionName, SessionId},
    tmux::{PROCESS_SAMPLE_SECONDS, PROCESS_SAMPLE_SEPARATOR, TmuxBackend},
};
use thiserror::Error;

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

/// Whether a workload is serving, as its own health command reported.
///
/// Kept apart from [`WorkloadStatus`], which answers whether the workload's
/// process is there. A live process that is not serving and a serving workload
/// whose liveness could not be confirmed are both real, and collapsing them
/// would lose the distinction the health command exists to make.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthStatus {
    /// The health command exited zero.
    Serving,
    /// The health command ran and reported a failure.
    NotServing { exit_status: Option<i32> },
    /// The health command could not be run, did not finish, or its result
    /// cannot be trusted. Never a pass.
    Unknown,
}

/// What a workload's processes are using on their host.
///
/// A workload is a `tmux` session, not one process: the pane's shell is usually
/// idle while a child does the work, so a figure that described only the pane
/// would report nothing for a workload compiling flat out. These are the totals
/// for every process under the workload's panes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceUsage {
    /// Processor share as the host reports it, summed over the workload's
    /// processes. Can exceed 100 on a multi-core host, which is the point.
    pub cpu_percent: f32,
    /// Resident memory in bytes, summed over the workload's processes.
    pub memory_bytes: u64,
}

/// What a workload is using, or that Tether could not find out.
///
/// Absence is a value here rather than a zero or an empty string: a host that
/// cannot report is not a workload using nothing, and the two would be
/// indistinguishable if this collapsed into numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResourceReport {
    Known(ResourceUsage),
    /// The host could not be asked, did not answer, answered unusably, or does
    /// not report the workload's processes at all.
    Unknown,
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
pub struct StatusWorkload {
    pub id: SessionId,
    /// The workload's directory, so a health command runs where it runs.
    pub directory: String,
    /// The configured health command, when the workload has one.
    pub health_command: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusHost {
    pub name: String,
    /// `None` means the local host; `Some` is a validated OpenSSH target.
    pub target: Option<String>,
    pub workloads: Vec<StatusWorkload>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusRequest {
    pub generation: u64,
    pub hosts: Vec<StatusHost>,
    /// Whether to ask each reachable host what its workloads are using.
    ///
    /// Opt-in because it costs two more commands per host, and a caller that
    /// does not display figures should not pay for them: `snapshot` asks only
    /// what it reports.
    pub resources: bool,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum StatusRequestError {
    #[error("status host count {actual} exceeds limit {maximum}")]
    TooManyHosts { actual: usize, maximum: usize },
    #[error("status workload count {actual} exceeds limit {maximum}")]
    TooManyWorkloads { actual: usize, maximum: usize },
}

#[derive(Clone, Debug, PartialEq)]
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
    Health {
        generation: u64,
        id: SessionId,
        status: HealthStatus,
        checked_at: SystemTime,
    },
    Resources {
        generation: u64,
        id: SessionId,
        report: ResourceReport,
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
            | Self::Health { generation, .. }
            | Self::Resources { generation, .. }
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

    pub fn try_start(&self, mut request: StatusRequest) -> Result<StatusRun, StatusRequestError> {
        validate_status_request(&request)?;
        request.hosts = normalize_status_hosts(request.hosts);
        Ok(self.start_validated(request))
    }

    fn start_validated(&self, request: StatusRequest) -> StatusRun {
        // Three messages per workload now: liveness, health where configured, and
        // usage where asked for. Sized so a worker's send never blocks, because a
        // blocked send stops observing the cancellation flag.
        // One Host and one Catalog per host, and per workload one liveness plus
        // at most one health and one usage result. Sized from the request so the
        // guarantee costs what this run needs rather than what the largest
        // conceivable run would.
        let workloads: usize = request.hosts.iter().map(|host| host.workloads.len()).sum();
        let (sender, receiver) = mpsc::sync_channel(workloads * 3 + request.hosts.len() * 2 + 1);
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
            let resources = request.resources;
            handles.push(thread::spawn(move || {
                worker_loop(
                    generation, jobs, &sender, &cancelled, &binaries, timeout, resources,
                );
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
            if workload_sets[index].insert(workload.id) {
                normalized[index].workloads.push(workload);
            }
        }
    }
    normalized
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
    resources: bool,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(host) = jobs.lock().expect("status jobs lock").pop_front() else {
            return;
        };
        probe_host(
            generation, host, sender, cancelled, binaries, timeout, resources,
        );
    }
}

fn probe_host(
    generation: u64,
    host: StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    timeout: Duration,
    resources: bool,
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
    let running = classified
        .workloads
        .iter()
        .any(|(_, status)| matches!(status, WorkloadStatus::Running { .. }));
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
                host: host.name.clone(),
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
    // A host Tether could not reach cannot answer a probe either, and the
    // liveness attempt just proved it: retrying per workload would spend the
    // whole refresh re-learning the same failure.
    if classified.reachability == HostReachability::Reachable {
        probe_health(generation, &host, sender, cancelled, binaries, timeout);
        // Only a running workload can be using anything, and asking a host whose
        // workloads have all ended would spend two commands on rows that cannot
        // show a figure.
        if resources && running {
            probe_resources(generation, &host, sender, cancelled, binaries, timeout);
        }
    } else {
        report_unknown_health(generation, &host, sender, cancelled);
        if resources {
            // A host that could not be reached cannot report what its workloads
            // are using either, and that is different from them using nothing.
            report_unknown_resources(generation, &host, sender, cancelled);
        }
    }
}

/// How long a health command may take before its result is unknown.
///
/// Short on purpose: a probe is answering "is this serving right now", and the
/// picker is waiting on it. A probe that needs longer has not answered.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs each configured health command and reports what it said.
///
/// Only for workloads that have one, so this costs nothing for the workloads
/// that do not. The result is never derived from liveness: a workload whose
/// process is missing still reports whatever its own probe says, and a probe
/// that cannot run reports unknown rather than borrowing the process's answer.
fn probe_health(
    generation: u64,
    host: &StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    timeout: Duration,
) {
    let health_timeout = HEALTH_TIMEOUT.min(timeout);
    // One deadline for the whole phase, so a host's cost stays bounded however
    // many workloads it has. A probe that does not fit reports unknown rather
    // than holding a worker while other hosts wait on it.
    let deadline = Instant::now() + health_timeout;
    for workload in &host.workloads {
        let Some(command) = workload.health_command.as_deref() else {
            continue;
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = if remaining.is_zero() {
            // The phase's budget is spent, so this probe never ran.
            HealthStatus::Unknown
        } else {
            match health_spec(
                host.target.as_deref(),
                &workload.directory,
                command,
                binaries,
            ) {
                Ok(spec) => classify_health(
                    run_bounded(&spec, remaining, cancelled),
                    host.target.is_some(),
                ),
                Err(()) => HealthStatus::Unknown,
            }
        };
        if matches!(status, HealthStatus::Unknown) && cancelled.load(Ordering::Acquire) {
            return;
        }
        if sender
            .send(StatusMessage::Health {
                generation,
                id: workload.id,
                status,
                checked_at: SystemTime::now(),
            })
            .is_err()
        {
            return;
        }
    }
}

/// Reports every configured probe as unknown without running any of them.
///
/// For a host Tether could not reach: the workloads may well be serving, and
/// saying so either way would be a claim about the workload drawn from a fact
/// about the host.
fn report_unknown_health(
    generation: u64,
    host: &StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
) {
    for workload in &host.workloads {
        if workload.health_command.is_none() {
            continue;
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if sender
            .send(StatusMessage::Health {
                generation,
                id: workload.id,
                status: HealthStatus::Unknown,
                checked_at: SystemTime::now(),
            })
            .is_err()
        {
            return;
        }
    }
}

/// How long a host's whole resource phase may take.
///
/// Two commands, however many workloads the host has, so this is a ceiling on
/// the phase rather than a per-workload allowance. Short because it answers a
/// question about right now, and the picker is waiting on it.
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Asks a reachable host what its workloads' processes are using.
///
/// Two commands for the whole host: which process each pane belongs to, and the
/// host's process table. Every workload's total is then summed locally from the
/// panes down, so a host with twenty workloads costs the same as one with one.
/// Anything that stops either command, or leaves a workload's processes
/// unidentifiable, reports [`ResourceReport::Unknown`] for that workload rather
/// than a figure it cannot stand behind.
fn probe_resources(
    generation: u64,
    host: &StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    timeout: Duration,
) {
    if host.workloads.is_empty() {
        return;
    }
    let deadline = Instant::now() + RESOURCE_TIMEOUT.min(timeout);
    let usage = collect_resource_usage(host, cancelled, binaries, deadline);
    for workload in &host.workloads {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let report = usage
            .as_ref()
            .and_then(|usage| usage.get(&workload.id).copied())
            .map_or(ResourceReport::Unknown, ResourceReport::Known);
        if sender
            .send(StatusMessage::Resources {
                generation,
                id: workload.id,
                report,
                checked_at: SystemTime::now(),
            })
            .is_err()
        {
            return;
        }
    }
}

/// Reports every workload as unknown without asking the host anything.
fn report_unknown_resources(
    generation: u64,
    host: &StatusHost,
    sender: &SyncSender<StatusMessage>,
    cancelled: &AtomicBool,
) {
    for workload in &host.workloads {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if sender
            .send(StatusMessage::Resources {
                generation,
                id: workload.id,
                report: ResourceReport::Unknown,
                checked_at: SystemTime::now(),
            })
            .is_err()
        {
            return;
        }
    }
}

/// How much of a host's answer Tether will read for the process table.
///
/// Larger than the general capture cap because this output is bounded by the
/// host's process count rather than by anything a workload writes: a build box
/// with thousands of processes is exactly the machine someone asks this question
/// about, and discarding its answer would report every workload on it as unknown.
const MAX_PROCESS_TABLE_BYTES: usize = 1024 * 1024;

/// Runs the two commands and sums each owned workload's processes.
///
/// `None` means the host could not be asked at all, which is different from a
/// workload whose processes were simply not in the answer: the first makes every
/// workload unknown, the second only that one.
fn collect_resource_usage(
    host: &StatusHost,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    deadline: Instant,
) -> Option<HashMap<SessionId, ResourceUsage>> {
    let backend = match host.target.as_deref() {
        Some(target) => TmuxBackend::remote(target.to_owned(), binaries.clone()).ok()?,
        None => TmuxBackend::local(binaries.clone()),
    };
    let panes = run_within(
        &backend.pane_pids_spec().ok()?,
        cancelled,
        deadline,
        MAX_CAPTURE_BYTES,
    )?;
    let samples = run_within(
        &backend.process_samples_spec().ok()?,
        cancelled,
        deadline,
        MAX_PROCESS_TABLE_BYTES,
    )?;
    Some(sum_workload_usage(
        &parse_pane_pids(&panes),
        &parse_process_samples(&samples)?,
    ))
}

/// Runs one command inside what is left of the phase's deadline.
fn run_within(
    spec: &CommandSpec,
    cancelled: &AtomicBool,
    deadline: Instant,
    max_capture: usize,
) -> Option<Vec<u8>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    match run_bounded_with_capture(spec, remaining, cancelled, max_capture) {
        BoundedOutput::Completed {
            status,
            stdout,
            stdout_truncated: false,
            ..
        } if status.success() => Some(stdout),
        // Anything else is an answer Tether cannot use: a truncated table would
        // silently drop processes and undercount the workloads that own them.
        _ => None,
    }
}

/// One process as the host reported it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProcessEntry {
    parent: u32,
    cpu_percent: f32,
    memory_bytes: u64,
}

/// Maps each owned session to the processes its panes were started for.
fn parse_pane_pids(stdout: &[u8]) -> HashMap<SessionId, Vec<u32>> {
    let mut panes: HashMap<SessionId, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some((name, pid)) = line.rsplit_once(':') else {
            continue;
        };
        // Only Tether's own sessions: a pane belonging to something else is not
        // a workload this answer is about.
        let Ok(id) = name.trim().parse::<SessionId>() else {
            continue;
        };
        if let Ok(pid) = pid.trim().parse::<u32>() {
            panes.entry(id).or_default().push(pid);
        }
    }
    panes
}

/// Reads `[[dd-]hh:]mm:ss` cumulative processor time into seconds.
fn parse_cpu_time(field: &str) -> Option<f64> {
    let (days, clock) = match field.split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().ok()?, clock),
        None => (0.0, field),
    };
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// Differences two processor-time samples into what each process is using now.
///
/// The first sample carries the parentage and resident size; the second exists
/// only to say how much processor time each process consumed while Tether
/// waited. A process that appears in one sample and not the other is left out
/// rather than counted as idle or as having used everything.
fn parse_process_samples(stdout: &[u8]) -> Option<HashMap<u32, ProcessEntry>> {
    let text = String::from_utf8_lossy(stdout);
    let (first, second) = text.split_once(PROCESS_SAMPLE_SEPARATOR)?;
    let mut later: HashMap<u32, f64> = HashMap::new();
    for line in second.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(pid), Some(time)) = (fields.next(), fields.next())
            && let (Ok(pid), Some(time)) = (pid.parse::<u32>(), parse_cpu_time(time))
        {
            later.insert(pid, time);
        }
    }
    let interval = PROCESS_SAMPLE_SECONDS as f64;
    let mut processes = HashMap::new();
    for line in first.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(parent), Some(time), Some(rss)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(parent), Some(time), Ok(rss)) = (
            pid.parse::<u32>(),
            parent.parse::<u32>(),
            parse_cpu_time(time),
            rss.parse::<u64>(),
        ) else {
            continue;
        };
        let Some(used) = later.get(&pid).map(|later| later - time) else {
            continue;
        };
        // A negative delta means the pid was reused between samples, so neither
        // figure describes one process.
        if !used.is_finite() || used < 0.0 || !rss_is_plausible(rss) {
            continue;
        }
        processes.insert(
            pid,
            ProcessEntry {
                parent,
                cpu_percent: (used / interval * 100.0) as f32,
                memory_bytes: rss.saturating_mul(1024),
            },
        );
    }
    Some(processes)
}

/// Whether a resident-size field is a figure rather than nonsense.
fn rss_is_plausible(rss: u64) -> bool {
    // A pathological `ps` could report a size larger than any real machine has;
    // saturating the sum would then hide it behind a plausible-looking total.
    rss < u64::MAX / 1024
}

/// Sums each workload's processes, panes and everything under them.
///
/// The pane's own shell is usually idle while a child does the work, so a total
/// that stopped at the pane would report a busy workload as using nothing.
fn sum_workload_usage(
    panes: &HashMap<SessionId, Vec<u32>>,
    processes: &HashMap<u32, ProcessEntry>,
) -> HashMap<SessionId, ResourceUsage> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, entry) in processes {
        children.entry(entry.parent).or_default().push(*pid);
    }
    let mut usage = HashMap::new();
    for (id, roots) in panes {
        let mut cpu_percent = 0.0;
        let mut memory_bytes = 0u64;
        let mut seen: HashSet<u32> = HashSet::new();
        let mut pending: Vec<u32> = roots.clone();
        let mut found = false;
        while let Some(pid) = pending.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(entry) = processes.get(&pid) {
                found = true;
                cpu_percent += entry.cpu_percent;
                memory_bytes = memory_bytes.saturating_add(entry.memory_bytes);
            }
            if let Some(descendants) = children.get(&pid) {
                pending.extend(descendants.iter().copied());
            }
        }
        // A pane whose processes are all gone is not a workload using nothing;
        // it is a workload this answer says nothing about.
        if found {
            usage.insert(
                *id,
                ResourceUsage {
                    cpu_percent,
                    memory_bytes,
                },
            );
        }
    }
    usage
}

/// The command that runs a workload's health check where the workload runs.
///
/// The same shape the workload itself was launched with: a login shell that
/// enters the directory and runs the configured command, locally or over the
/// host's existing SSH connection. It does not run inside the workload's own
/// pane, so it cannot disturb the work it is asking about.
fn health_spec(
    target: Option<&str>,
    directory: &str,
    command: &str,
    binaries: &ProcessBinaries,
) -> Result<CommandSpec, ()> {
    let backend = match target {
        Some(target) => TmuxBackend::remote(target.to_owned(), binaries.clone()).map_err(|_| ())?,
        None => TmuxBackend::local(binaries.clone()),
    };
    backend
        .directory_shell_spec(directory, command)
        .map_err(|_| ())
}

/// The status `ssh` reserves for its own transport failures.
const SSH_FAILURE: i32 = 255;

/// What a finished probe said about the workload, if anything.
///
/// `remote` is needed because the answer travels over SSH, and SSH reserves
/// `255` for its own transport failures. Reading that as the workload's verdict
/// would report a service as down when Tether never reached the machine.
fn classify_health(result: BoundedOutput, remote: bool) -> HealthStatus {
    match result {
        BoundedOutput::Completed { status, .. } if status.success() => HealthStatus::Serving,
        // A probe that could not be executed, whose command was not found, or
        // whose transport failed has observed nothing. Reporting "not serving"
        // there would be an inference about the workload from a fact about the
        // probe.
        BoundedOutput::Completed { status, .. }
            if status.code().is_some_and(|code| {
                crate::tmux::HEALTH_UNRUNNABLE.contains(&code) || (remote && code == SSH_FAILURE)
            }) =>
        {
            HealthStatus::Unknown
        }
        // A probe killed by a signal never reported either. `None` here means
        // something outside Tether reaped it, since a probe over its own
        // deadline arrives as `TimedOut`.
        BoundedOutput::Completed { status, .. } => match status.code() {
            Some(exit_status) => HealthStatus::NotServing {
                exit_status: Some(exit_status),
            },
            None => HealthStatus::Unknown,
        },
        // A probe that did not finish or could not start says nothing either.
        BoundedOutput::TimedOut
        | BoundedOutput::Cancelled
        | BoundedOutput::SpawnError(_)
        | BoundedOutput::Error => HealthStatus::Unknown,
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
    let requested = host
        .workloads
        .iter()
        .map(|workload| workload.id)
        .collect::<HashSet<_>>();
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
                    .map(|workload| {
                        let status = catalog.owned.get(&workload.id).map_or(
                            WorkloadStatus::Missing,
                            |attached| WorkloadStatus::Running {
                                attached: *attached,
                            },
                        );
                        (workload.id, status)
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
            if host.target.is_some() && status.code() == Some(SSH_FAILURE) =>
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
    workloads: &[StatusWorkload],
    status: WorkloadStatus,
) -> Vec<(SessionId, WorkloadStatus)> {
    workloads
        .iter()
        .map(|workload| (workload.id, status))
        .collect()
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
    run_bounded_with_capture(spec, timeout, cancelled, MAX_CAPTURE_BYTES)
}

/// Runs a bounded command, retaining at most `max_capture` bytes per stream.
///
/// The ceiling is a parameter because it is a judgement about the output, not
/// about the command: a workload's terminal text is untrusted and kept small,
/// while a host's process table is bounded by how many processes it has and
/// discarding it would report every workload on a busy host as unknown.
pub(crate) fn run_bounded_with_capture(
    spec: &CommandSpec,
    timeout: Duration,
    cancelled: &AtomicBool,
    max_capture: usize,
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
        if drain_pipe(stdout.as_mut(), &mut stdout_capture, max_capture).is_err()
            || drain_pipe(stderr.as_mut(), &mut stderr_capture, max_capture).is_err()
        {
            terminate_child(&mut child);
            return BoundedOutput::Error;
        }
        if cancelled.load(Ordering::Acquire) {
            terminate_child(&mut child);
            let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture, max_capture);
            let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture, max_capture);
            return BoundedOutput::Cancelled;
        }
        // `try_wait` polls with `WNOHANG` and, unlike `Child::wait`, reports an
        // interruption instead of repeating the `waitpid`. Treating that as a
        // failed probe killed a healthy child. The retry stays inside the
        // deadline this loop already enforces, and a signal that arrives with
        // the deadline already due is reported as the timeout it is.
        let waited = interrupt::retry_interrupted(Budget::Until(deadline), || child.try_wait());
        match waited {
            Ok(Some(status)) => {
                // The direct child may have successfully forked descendants
                // into its fresh process group. End them before returning so a
                // completed bounded command cannot leave orphaned work behind.
                kill_process_group(child.id());
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture, max_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture, max_capture);
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
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture, max_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture, max_capture);
                return BoundedOutput::TimedOut;
            }
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            // A poll whose retry budget was the command deadline reports
            // `TimedOut` when that deadline is due, which is the same outcome as
            // the arm above rather than an unexplained transport failure.
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                terminate_child(&mut child);
                let _ = drain_pipe(stdout.as_mut(), &mut stdout_capture, max_capture);
                let _ = drain_pipe(stderr.as_mut(), &mut stderr_capture, max_capture);
                return BoundedOutput::TimedOut;
            }
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

fn drain_pipe<R: Read>(
    pipe: Option<&mut R>,
    capture: &mut Capture,
    max_capture: usize,
) -> io::Result<()> {
    let Some(pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    let mut drained = 0;
    loop {
        // The pipes are non-blocking, so a retried read cannot wait; the
        // interruption retry itself lives in `crate::interrupt`.
        match interrupt::retry_interrupted(Budget::Immediate, || pipe.read(&mut buffer)) {
            Ok(0) => return Ok(()),
            Ok(length) => {
                drained += length;
                let remaining = max_capture.saturating_sub(capture.bytes.len());
                let retained = remaining.min(length);
                capture.bytes.extend_from_slice(&buffer[..retained]);
                capture.truncated |= retained < length;
                if drained >= MAX_DRAIN_BYTES_PER_TICK {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
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

    /// A child pipe that reports an interruption before the output it has, then
    /// reports that it has nothing more for now, as a non-blocking pipe does.
    struct InterruptedPipe {
        remaining: &'static [u8],
        interrupted: bool,
    }

    impl Read for InterruptedPipe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            if self.remaining.is_empty() {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let length = buffer.len().min(self.remaining.len());
            buffer[..length].copy_from_slice(&self.remaining[..length]);
            self.remaining = &self.remaining[length..];
            Ok(length)
        }
    }

    #[test]
    fn draining_a_pipe_retries_a_read_interrupted_by_a_signal() {
        // A signal arriving while a bounded `tmux` or SSH command is being
        // drained says nothing about the command. Reporting it discarded the
        // output and killed a child that was working.
        let mut pipe = InterruptedPipe {
            remaining: b"session\n",
            interrupted: false,
        };
        let mut capture = Capture::default();
        drain_pipe(Some(&mut pipe), &mut capture, MAX_CAPTURE_BYTES)
            .expect("an interruption is not a failure");
        assert_eq!(capture.bytes, b"session\n");
        assert!(!capture.truncated);
    }

    #[test]
    fn draining_a_pipe_still_reports_a_real_read_failure() {
        struct Broken;

        impl Read for Broken {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }

        let mut capture = Capture::default();
        let error = drain_pipe(Some(&mut Broken), &mut capture, MAX_CAPTURE_BYTES).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }

    #[cfg(unix)]
    fn health_result(command: &str, directory: &str) -> HealthStatus {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        let cancelled = AtomicBool::new(false);
        let host = StatusHost {
            name: "local".to_owned(),
            target: None,
            workloads: vec![StatusWorkload {
                id: "tether-0197f198000070008000000000000001".parse().unwrap(),
                directory: directory.to_owned(),
                health_command: Some(command.to_owned()),
            }],
        };
        probe_health(
            7,
            &host,
            &sender,
            &cancelled,
            &ProcessBinaries::new(temp.path().join("ssh"), temp.path().join("tmux")),
            Duration::from_secs(5),
        );
        drop(sender);
        match receiver.recv().expect("a configured probe always reports") {
            StatusMessage::Health { status, .. } => status,
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_health_command_answers_with_its_exit_status() {
        assert_eq!(health_result("exit 0", "/"), HealthStatus::Serving);
        assert_eq!(
            health_result("exit 3", "/"),
            HealthStatus::NotServing {
                exit_status: Some(3)
            }
        );
    }

    /// A finished probe carrying an exact exit status.
    #[cfg(unix)]
    fn completed_with(code: i32) -> BoundedOutput {
        let status = Command::new("/bin/sh")
            .args(["-c", &format!("exit {code}")])
            .status()
            .unwrap();
        BoundedOutput::Completed {
            status,
            stdout: Vec::new(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
        }
    }

    /// A probe reaped by a signal, which reports no exit code on Unix.
    #[cfg(unix)]
    fn killed_by_signal() -> BoundedOutput {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(status.code().is_none(), "{status:?}");
        BoundedOutput::Completed {
            status,
            stdout: Vec::new(),
            stdout_truncated: false,
            stderr: Vec::new(),
            stderr_truncated: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_health_command_that_cannot_run_is_unknown_rather_than_a_pass() {
        // A probe that cannot reach its directory has not observed anything, and
        // "I could not look" must never read as "it is serving".
        assert_eq!(
            health_result("exit 0", "/definitely/not/a/directory"),
            HealthStatus::Unknown,
        );
        for result in [
            BoundedOutput::TimedOut,
            BoundedOutput::SpawnError(io::ErrorKind::NotFound),
            BoundedOutput::Error,
            BoundedOutput::Cancelled,
        ] {
            assert_eq!(classify_health(result, false), HealthStatus::Unknown);
        }

        // SSH reserves 255 for its own failures, so a remote probe that exits
        // 255 never reached the workload. The same status from a local probe is
        // the command's own answer.
        let transport = completed_with(SSH_FAILURE);
        assert_eq!(classify_health(transport, true), HealthStatus::Unknown);
        assert_eq!(
            classify_health(completed_with(SSH_FAILURE), false),
            HealthStatus::NotServing {
                exit_status: Some(SSH_FAILURE)
            }
        );

        // A probe reaped by something outside Tether reported nothing at all.
        assert_eq!(
            classify_health(killed_by_signal(), false),
            HealthStatus::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_workload_without_a_health_command_is_never_probed() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        let cancelled = AtomicBool::new(false);
        let host = StatusHost {
            name: "local".to_owned(),
            target: None,
            workloads: vec![StatusWorkload {
                id: "tether-0197f198000070008000000000000001".parse().unwrap(),
                directory: "/".to_owned(),
                health_command: None,
            }],
        };
        probe_health(
            7,
            &host,
            &sender,
            &cancelled,
            &ProcessBinaries::new(temp.path().join("ssh"), temp.path().join("tmux")),
            Duration::from_secs(5),
        );
        drop(sender);
        assert!(
            receiver.recv().is_err(),
            "a workload with no probe must report nothing about serving"
        );
    }

    fn session(tail: u8) -> SessionId {
        format!("tether-0197f1980000700080000000000000{tail:02}")
            .parse()
            .unwrap()
    }

    #[test]
    fn a_workload_totals_every_process_under_its_panes() {
        // The pane's shell is idle; the compiler underneath it is not. A total
        // that stopped at the pane would report this workload as using nothing,
        // which is the whole failure this sums past.
        let panes = HashMap::from([(session(1), vec![100]), (session(2), vec![200, 201])]);
        let processes = HashMap::from([
            (
                100,
                ProcessEntry {
                    parent: 1,
                    cpu_percent: 0.1,
                    memory_bytes: 2 * 1024 * 1024,
                },
            ),
            (
                101,
                ProcessEntry {
                    parent: 100,
                    cpu_percent: 90.0,
                    memory_bytes: 500 * 1024 * 1024,
                },
            ),
            (
                102,
                ProcessEntry {
                    parent: 101,
                    cpu_percent: 10.0,
                    memory_bytes: 10 * 1024 * 1024,
                },
            ),
            (
                200,
                ProcessEntry {
                    parent: 1,
                    cpu_percent: 1.0,
                    memory_bytes: 1024 * 1024,
                },
            ),
            (
                201,
                ProcessEntry {
                    parent: 1,
                    cpu_percent: 2.0,
                    memory_bytes: 1024 * 1024,
                },
            ),
        ]);

        let usage = sum_workload_usage(&panes, &processes);
        let first = usage.get(&session(1)).expect("the busy workload is summed");
        assert!(
            (first.cpu_percent - 100.1).abs() < 0.01,
            "the pane and everything under it: {first:?}"
        );
        assert_eq!(first.memory_bytes, 512 * 1024 * 1024);
        // A session with two panes is one workload.
        let second = usage.get(&session(2)).expect("both panes are one workload");
        assert!((second.cpu_percent - 3.0).abs() < 0.01, "{second:?}");
        assert_eq!(second.memory_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn a_workload_whose_processes_are_gone_is_absent_rather_than_zero() {
        // Nothing under this pane is in the process table any more. Reporting
        // zero would say the workload is idle; it has to say nothing at all so
        // the caller can report it as unknown.
        let panes = HashMap::from([(session(1), vec![999])]);
        let usage = sum_workload_usage(&panes, &HashMap::new());
        assert!(
            !usage.contains_key(&session(1)),
            "an absent process is not a zero figure: {usage:?}"
        );
    }

    #[test]
    fn processor_share_is_what_was_used_between_the_samples() {
        // `ps` reports %CPU as an average over a process's whole life, so a
        // workload that has been up for days and starts eating a core would
        // round to nothing. Differencing two samples asks what it is using now.
        let samples = format!(
            "  100     1  00:10 4096\n\
               101   100  1-02:03:04 2048\n\
             {PROCESS_SAMPLE_SEPARATOR}\n\
               100        00:11\n\
               101        1-02:03:04\n"
        );
        let processes = parse_process_samples(samples.as_bytes()).unwrap();
        assert_eq!(processes.len(), 2, "{processes:?}");
        // One second of processor time over a one second wait is a full core,
        // whatever the process did before Tether looked.
        let busy = processes.get(&100).unwrap();
        assert!((busy.cpu_percent - 100.0).abs() < 0.01, "{busy:?}");
        assert_eq!(busy.memory_bytes, 4096 * 1024);
        // A day of accumulated time and no movement is idle now.
        let idle = processes.get(&101).unwrap();
        assert!(idle.cpu_percent.abs() < 0.01, "{idle:?}");
    }

    #[test]
    fn a_process_row_is_read_or_ignored_but_never_guessed() {
        let samples = format!(
            "  100     1  00:00 4096\n\
               garbage row\n\
               102   100  notanumber 4\n\
               103   100  00:00 2048\n\
               104   100  00:20 2048\n\
             {PROCESS_SAMPLE_SEPARATOR}\n\
               100        00:01\n\
               102        00:01\n\
               104        00:10\n"
        );
        let processes = parse_process_samples(samples.as_bytes()).unwrap();
        // 103 is missing from the second sample, so nothing is known about what
        // it used; 104 went backwards, which means the pid was reused.
        assert_eq!(
            processes.keys().copied().collect::<Vec<_>>(),
            vec![100],
            "{processes:?}"
        );
    }

    #[test]
    fn output_without_both_samples_is_no_answer_at_all() {
        // A host whose second sample never arrived has told Tether nothing it can
        // difference, and a figure from the first alone would be the lifetime
        // average this deliberately avoids.
        assert!(parse_process_samples(b"100 1 00:10 4096\n").is_none());
    }

    #[test]
    fn only_tether_owned_panes_are_matched_to_workloads() {
        let panes = parse_pane_pids(
            format!(
                "{}:100\nsomeone-elses-session:200\n{}:notapid\n",
                session(1),
                session(2)
            )
            .as_bytes(),
        );
        assert_eq!(panes.len(), 1, "{panes:?}");
        assert_eq!(panes.get(&session(1)), Some(&vec![100]));
    }
}
