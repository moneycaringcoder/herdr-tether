use std::{
    collections::{BinaryHeap, HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    backend::{CommandSpec, ProcessBinaries},
    sshcfg::{openssh_connection_args, openssh_target},
    status::{BoundedOutput, run_bounded},
};
const MAX_DISCOVERY_WORKERS: usize = 16;
const MAX_DISCOVERY_ENTRIES: usize = 100_000;
const MAX_DISCOVERY_RESULTS: usize = 4_096;
const MAX_DISCOVERY_LOCATIONS: usize = 256;
const MAX_DISCOVERY_ROOTS: usize = 256;
const MAX_DISCOVERY_MESSAGES: usize =
    MAX_DISCOVERY_RESULTS + MAX_DISCOVERY_ROOTS + MAX_DISCOVERY_LOCATIONS + 1;

const REMOTE_SCAN_SCRIPT: &str = r#"max_depth=$1
result_limit=$2
entry_limit=$3
shift 3
LC_ALL=C
export LC_ALL
entries=0
results=0
listing_index=0
state_dir=${TMPDIR:-/tmp}/tether-discovery.$$
(umask 077 && mkdir "$state_dir") || exit 1
trap 'rm -rf "$state_dir"' 0 1 2 3 15

scan() {
  if [ "$entries" -ge "$entry_limit" ]; then
    printf 'N\000'
    exit 0
  fi
  entries=$((entries + 1))
  if [ -L "$1" ] || [ ! -d "$1" ]; then
    return
  fi
  if [ ! -L "$1/.git" ] && { [ -d "$1/.git" ] || [ -f "$1/.git" ]; }; then
    if [ "$results" -ge "$result_limit" ]; then
      printf 'L\000'
      exit 0
    fi
    printf 'R\000%s\000%s\000' "$4" "$3"
    results=$((results + 1))
    return
  fi
  if [ "$2" -ge "$max_depth" ]; then
    return
  fi

  remaining=$((entry_limit - entries))
  listing_index=$((listing_index + 1))
  listing=$state_dir/$listing_index
  find_root=$1
  case "$find_root" in -*) find_root=./$find_root ;; esac
  find "$find_root" -mindepth 1 -maxdepth 1 -print |
    head -n "$((remaining + 1))" |
    sort > "$listing"
  while IFS= read -r child; do
    if [ ! -e "$child" ] && [ ! -L "$child" ]; then
      continue
    fi
    name=${child##*/}
    if [ -n "$3" ]; then
      next=$3/$name
    else
      next=$name
    fi
    scan "$child" "$(($2 + 1))" "$next" "$4"
  done < "$listing"
  rm -f "$listing"
}

index=0
for configured_root do
  case "$configured_root" in
    '~') root=$HOME ;;
    '~/'*) root=$HOME/${configured_root#\~/} ;;
    *) root=$configured_root ;;
  esac
  if [ -L "$root" ] || [ ! -d "$root" ]; then
    printf 'E\000%s\000' "$index"
  else
    scan "$root" 0 '' "$index"
  fi
  index=$((index + 1))
done
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryCompletion {
    Complete,
    ResultsLimit,
    EntriesLimit,
    TimedOut,
    Unavailable,
    OutputLimit,
    Malformed,
    Error,
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryRunCompletion {
    Complete = 1,
    ResultsLimit = 2,
    EntriesLimit = 3,
    TimedOut = 4,
    Unavailable = 5,
    OutputLimit = 6,
    Malformed = 7,
    Error = 8,
    Cancelled = 9,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryMessage {
    Repository {
        generation: u64,
        host: String,
        path: String,
    },
    RootError {
        generation: u64,
        host: String,
        root: String,
    },
    HostFinished {
        generation: u64,
        host: String,
        completion: DiscoveryCompletion,
    },
    Finished {
        generation: u64,
    },
}

impl DiscoveryMessage {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Repository { generation, .. }
            | Self::RootError { generation, .. }
            | Self::HostFinished { generation, .. }
            | Self::Finished { generation } => *generation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryLocation {
    pub host: String,
    pub target: Option<String>,
    pub roots: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRequest {
    pub generation: u64,
    pub locations: Vec<DiscoveryLocation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_results: usize,
    pub timeout: Duration,
    pub workers: usize,
}

type DiscoveryClock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
pub struct DiscoveryService {
    binaries: ProcessBinaries,
    limits: DiscoveryLimits,
    clock: DiscoveryClock,
}

impl std::fmt::Debug for DiscoveryService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiscoveryService")
            .field("binaries", &self.binaries)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl DiscoveryService {
    pub fn new(binaries: ProcessBinaries, limits: DiscoveryLimits) -> Self {
        Self::new_with_clock(binaries, limits, Instant::now)
    }

    pub fn new_with_clock<F>(
        binaries: ProcessBinaries,
        mut limits: DiscoveryLimits,
        clock: F,
    ) -> Self
    where
        F: Fn() -> Instant + Send + Sync + 'static,
    {
        limits.workers = limits.workers.clamp(1, MAX_DISCOVERY_WORKERS);
        limits.max_entries = limits.max_entries.clamp(1, MAX_DISCOVERY_ENTRIES);
        limits.max_results = limits.max_results.clamp(1, MAX_DISCOVERY_RESULTS);
        Self {
            binaries,
            limits,
            clock: Arc::new(clock),
        }
    }

    pub fn start(&self, request: DiscoveryRequest) -> DiscoveryRun {
        let (sender, receiver) = mpsc::sync_channel(MAX_DISCOVERY_MESSAGES);
        let cancelled = Arc::new(AtomicBool::new(false));
        let completion = Arc::new(AtomicUsize::new(0));
        let generation = request.generation;
        let root_count = request
            .locations
            .iter()
            .try_fold(0usize, |count, location| {
                count.checked_add(location.roots.len())
            });
        if request.locations.len() > MAX_DISCOVERY_LOCATIONS
            || root_count.is_none_or(|count| count > MAX_DISCOVERY_ROOTS)
        {
            completion.store(DiscoveryRunCompletion::Error as usize, Ordering::Release);
            let _ = sender.send(DiscoveryMessage::Finished { generation });
            return DiscoveryRun {
                receiver,
                cancelled,
                completion,
            };
        }

        let location_count = request.locations.len();
        let worker_count = self.limits.workers.min(location_count.max(1));
        let mut outcome_receivers = Vec::with_capacity(location_count);
        let mut queued_jobs = VecDeque::with_capacity(location_count);
        for location in request.locations {
            // A single pending message per location gives ordered publication
            // backpressure without allowing a slow early location to make later
            // locations accumulate whole scan outcomes.
            let (outcomes, receiver) = mpsc::sync_channel(1);
            queued_jobs.push_back(DiscoveryJob { location, outcomes });
            outcome_receivers.push(receiver);
        }
        let jobs = Arc::new(Mutex::new(JobQueue {
            jobs: queued_jobs,
            reservations: EntryReservations::new(self.limits.max_entries),
        }));
        let started = (self.clock)();
        let deadline = started.checked_add(self.limits.timeout).unwrap_or(started);
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let context = WorkerContext {
                generation,
                cancelled: Arc::clone(&cancelled),
                binaries: self.binaries.clone(),
                limits: self.limits,
                deadline,
                clock: Arc::clone(&self.clock),
            };
            handles.push(thread::spawn(move || worker_loop(jobs, &context)));
        }
        let supervisor_cancelled = Arc::clone(&cancelled);
        let supervisor_completion = Arc::clone(&completion);
        let max_results = self.limits.max_results;
        thread::spawn(move || {
            let outcome = publish_outcomes(
                generation,
                outcome_receivers,
                &sender,
                &supervisor_cancelled,
                max_results,
            );
            let _ = supervisor_completion.compare_exchange(
                0,
                outcome as usize,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            supervisor_cancelled.store(true, Ordering::Release);
            for handle in handles {
                let _ = handle.join();
            }
            let _ = sender.send(DiscoveryMessage::Finished { generation });
        });
        DiscoveryRun {
            receiver,
            cancelled,
            completion,
        }
    }
}

pub struct DiscoveryRun {
    pub receiver: Receiver<DiscoveryMessage>,
    cancelled: Arc<AtomicBool>,
    completion: Arc<AtomicUsize>,
}

impl DiscoveryRun {
    pub fn cancel(&self) {
        let _ = self.completion.compare_exchange(
            0,
            DiscoveryRunCompletion::Cancelled as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn completion(&self) -> Option<DiscoveryRunCompletion> {
        match self.completion.load(Ordering::Acquire) {
            0 => None,
            1 => Some(DiscoveryRunCompletion::Complete),
            2 => Some(DiscoveryRunCompletion::ResultsLimit),
            3 => Some(DiscoveryRunCompletion::EntriesLimit),
            4 => Some(DiscoveryRunCompletion::TimedOut),
            5 => Some(DiscoveryRunCompletion::Unavailable),
            6 => Some(DiscoveryRunCompletion::OutputLimit),
            7 => Some(DiscoveryRunCompletion::Malformed),
            8 => Some(DiscoveryRunCompletion::Error),
            9 => Some(DiscoveryRunCompletion::Cancelled),
            _ => unreachable!("invalid discovery completion"),
        }
    }
}

impl Drop for DiscoveryRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct WorkerContext {
    generation: u64,
    cancelled: Arc<AtomicBool>,
    binaries: ProcessBinaries,
    limits: DiscoveryLimits,
    deadline: Instant,
    clock: DiscoveryClock,
}

struct DiscoveryJob {
    location: DiscoveryLocation,
    outcomes: SyncSender<DiscoveryMessage>,
}

// Entry allowances are assigned while jobs are dequeued, so worker timing
// cannot change which request index owns each allowance. Earlier locations
// receive any indivisible remainder. An unused allowance is intentionally not
// returned: bounded under-utilization is preferable to timing-dependent theft
// by a later, faster scan.
struct JobQueue {
    jobs: VecDeque<DiscoveryJob>,
    reservations: EntryReservations,
}

impl JobQueue {
    fn pop(&mut self) -> Option<(DiscoveryJob, usize)> {
        let remaining_jobs = self.jobs.len();
        let job = self.jobs.pop_front()?;
        Some((job, self.reservations.reserve(remaining_jobs)))
    }
}

struct EntryReservations {
    remaining: usize,
}

impl EntryReservations {
    fn new(max_entries: usize) -> Self {
        Self {
            remaining: max_entries,
        }
    }

    fn reserve(&mut self, remaining_jobs: usize) -> usize {
        let allowance = self.remaining.div_ceil(remaining_jobs);
        self.remaining -= allowance;
        allowance
    }
}

struct ScanContext<'a> {
    generation: u64,
    sender: &'a SyncSender<DiscoveryMessage>,
    cancelled: &'a AtomicBool,
    entries: &'a AtomicUsize,
    entry_limit: usize,
    limits: DiscoveryLimits,
    deadline: Instant,
    clock: &'a DiscoveryClock,
}
fn send_scan(context: &ScanContext<'_>, mut message: DiscoveryMessage) -> bool {
    loop {
        if context.cancelled.load(Ordering::Acquire) {
            return false;
        }
        match context.sender.try_send(message) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(returned)) => {
                message = returned;
                thread::yield_now();
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn worker_loop(jobs: Arc<Mutex<JobQueue>>, context: &WorkerContext) {
    loop {
        if context.cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some((DiscoveryJob { location, outcomes }, entry_limit)) =
            jobs.lock().expect("discovery jobs lock").pop()
        else {
            return;
        };
        let entries = AtomicUsize::new(0);
        let scan_context = ScanContext {
            generation: context.generation,
            sender: &outcomes,
            cancelled: &context.cancelled,
            entries: &entries,
            entry_limit,
            limits: context.limits,
            deadline: context.deadline,
            clock: &context.clock,
        };
        if entry_limit == 0 {
            let _ = send_scan(
                &scan_context,
                DiscoveryMessage::HostFinished {
                    generation: context.generation,
                    host: location.host,
                    completion: DiscoveryCompletion::EntriesLimit,
                },
            );
        } else {
            match &location.target {
                Some(target) => {
                    scan_remote(&location, target, &context.binaries, &scan_context);
                }
                None => scan_local(&location, &scan_context),
            }
        }
    }
}

fn publish_outcomes(
    generation: u64,
    outcomes: Vec<Receiver<DiscoveryMessage>>,
    sender: &SyncSender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    max_results: usize,
) -> DiscoveryRunCompletion {
    let mut overall = DiscoveryRunCompletion::Complete;
    let published_results = AtomicUsize::new(0);
    for outcome in outcomes {
        let mut exceeded_results = false;
        for message in outcome {
            if cancelled.load(Ordering::Acquire) {
                return DiscoveryRunCompletion::Cancelled;
            }
            match message {
                DiscoveryMessage::Repository { .. }
                    if published_results
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |results| {
                            (results < max_results).then_some(results + 1)
                        })
                        .is_err() =>
                {
                    exceeded_results = true;
                }
                DiscoveryMessage::Repository { .. } => {
                    if sender.send(message).is_err() {
                        return overall;
                    }
                }
                DiscoveryMessage::HostFinished {
                    host, completion, ..
                } => {
                    let completion = if exceeded_results {
                        DiscoveryCompletion::ResultsLimit
                    } else {
                        completion
                    };
                    overall = more_significant_completion(overall, completion);
                    if sender
                        .send(DiscoveryMessage::HostFinished {
                            generation,
                            host,
                            completion,
                        })
                        .is_err()
                    {
                        return overall;
                    }
                }
                _ => {
                    if sender.send(message).is_err() {
                        return overall;
                    }
                }
            }
        }
        if exceeded_results {
            return DiscoveryRunCompletion::ResultsLimit;
        }
    }
    overall
}

fn more_significant_completion(
    current: DiscoveryRunCompletion,
    completion: DiscoveryCompletion,
) -> DiscoveryRunCompletion {
    let candidate = match completion {
        DiscoveryCompletion::Complete => DiscoveryRunCompletion::Complete,
        DiscoveryCompletion::ResultsLimit => DiscoveryRunCompletion::ResultsLimit,
        DiscoveryCompletion::EntriesLimit => DiscoveryRunCompletion::EntriesLimit,
        DiscoveryCompletion::TimedOut => DiscoveryRunCompletion::TimedOut,
        DiscoveryCompletion::Unavailable => DiscoveryRunCompletion::Unavailable,
        DiscoveryCompletion::OutputLimit => DiscoveryRunCompletion::OutputLimit,
        DiscoveryCompletion::Malformed => DiscoveryRunCompletion::Malformed,
        DiscoveryCompletion::Error => DiscoveryRunCompletion::Error,
    };
    if completion_priority(candidate) > completion_priority(current) {
        candidate
    } else {
        current
    }
}

fn completion_priority(completion: DiscoveryRunCompletion) -> usize {
    match completion {
        DiscoveryRunCompletion::Complete => 0,
        DiscoveryRunCompletion::Unavailable => 1,
        DiscoveryRunCompletion::Malformed => 2,
        DiscoveryRunCompletion::Error => 3,
        DiscoveryRunCompletion::OutputLimit => 4,
        DiscoveryRunCompletion::EntriesLimit => 5,
        DiscoveryRunCompletion::ResultsLimit => 6,
        DiscoveryRunCompletion::TimedOut => 7,
        DiscoveryRunCompletion::Cancelled => 8,
    }
}

struct LocalScan<'a> {
    context: &'a ScanContext<'a>,
    host: &'a str,
    results: usize,
    seen: HashSet<PathBuf>,
    completion: DiscoveryCompletion,
}

fn scan_local(location: &DiscoveryLocation, context: &ScanContext<'_>) {
    let mut scan = LocalScan {
        context,
        host: &location.host,
        results: 0,
        seen: HashSet::new(),
        completion: DiscoveryCompletion::Complete,
    };
    for root in &location.roots {
        if scan.stopped() {
            break;
        }
        let path = expand_local_root(root);
        let symlink_root =
            fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        let result = if symlink_root {
            Err(std::io::Error::other("repository scan root is a symlink"))
        } else {
            scan.visit(&path, &path, Path::new(root), 0)
        };
        if result.is_err() && !scan.context.cancelled.load(Ordering::Acquire) {
            scan.completion = DiscoveryCompletion::Error;
            let _ = send_scan(
                scan.context,
                DiscoveryMessage::RootError {
                    generation: scan.context.generation,
                    host: location.host.clone(),
                    root: root.clone(),
                },
            );
        }
    }
    if !context.cancelled.load(Ordering::Acquire) {
        let _ = send_scan(
            context,
            DiscoveryMessage::HostFinished {
                generation: context.generation,
                host: location.host.clone(),
                completion: scan.completion,
            },
        );
    }
}

impl LocalScan<'_> {
    fn stopped(&mut self) -> bool {
        if self.context.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if self.completion == DiscoveryCompletion::ResultsLimit {
            return true;
        }
        if (self.context.clock)() >= self.context.deadline {
            self.completion = DiscoveryCompletion::TimedOut;
            return true;
        }
        if self.context.entries.load(Ordering::Acquire) >= self.context.entry_limit {
            self.completion = DiscoveryCompletion::EntriesLimit;
            return true;
        }
        false
    }

    fn visit(
        &mut self,
        path: &Path,
        scan_root: &Path,
        display_root: &Path,
        depth: usize,
    ) -> std::io::Result<()> {
        if self.stopped() {
            return Ok(());
        }
        if self
            .context
            .entries
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |entries| {
                (entries < self.context.entry_limit).then_some(entries + 1)
            })
            .is_err()
        {
            self.completion = DiscoveryCompletion::EntriesLimit;
            return Ok(());
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(());
        }
        if is_repository(path)? {
            if self.seen.insert(path.to_path_buf()) {
                if self.results >= self.context.limits.max_results {
                    self.completion = DiscoveryCompletion::ResultsLimit;
                    return Ok(());
                }
                let relative = path.strip_prefix(scan_root).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "repository escaped its scan root",
                    )
                })?;
                let display_path = if relative.as_os_str().is_empty() {
                    display_root.to_path_buf()
                } else {
                    display_root.join(relative)
                };
                let path = display_path.to_str().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "repository path is not valid UTF-8",
                    )
                })?;
                self.results += 1;
                let _ = send_scan(
                    self.context,
                    DiscoveryMessage::Repository {
                        generation: self.context.generation,
                        host: self.host.to_owned(),
                        path: path.to_owned(),
                    },
                );
            }
            return Ok(());
        }
        if depth >= self.context.limits.max_depth {
            return Ok(());
        }
        let remaining_entries = self
            .context
            .entry_limit
            .saturating_sub(self.context.entries.load(Ordering::Acquire));
        let mut children = BinaryHeap::new();
        for entry in fs::read_dir(path)? {
            if self.stopped() {
                break;
            }
            match entry {
                Ok(entry) => {
                    retain_bounded_child(&mut children, entry.path(), remaining_entries);
                }
                Err(error) if ignorable_scan_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
        let mut children = children.into_vec();
        children.sort();
        for child in children {
            if self.stopped() {
                break;
            }
            if let Err(error) = self.visit(&child, scan_root, display_root, depth + 1)
                && !ignorable_scan_error(&error)
            {
                self.completion = DiscoveryCompletion::Error;
            }
        }
        Ok(())
    }
}

fn retain_bounded_child(children: &mut BinaryHeap<PathBuf>, child: PathBuf, capacity: usize) {
    if capacity == 0 {
        return;
    }
    if children.len() < capacity {
        children.push(child);
    } else if children.peek().is_some_and(|largest| child < *largest) {
        children.pop();
        children.push(child);
    }
}

fn ignorable_scan_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
}

fn expand_local_root(root: &str) -> PathBuf {
    if root == "~" {
        return std::env::var_os("HOME").map_or_else(|| PathBuf::from(root), PathBuf::from);
    }
    if let Some(rest) = root.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(root)
}

fn is_repository(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path.join(".git")) {
        Ok(metadata) => Ok(metadata.is_dir() || metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether a path is a Git checkout: a directory holding a `.git` entry.
///
/// Exposed because a reported worktree path has to be checked before the picker
/// offers it. Anything unreadable answers `false`: a directory that cannot be
/// inspected is not one to send a user into, and the caller's fallback is simply
/// not reordering.
///
/// Unlike the scanner, this follows symlinks. The scanner refuses a symlinked
/// root because a scan that follows one can leave the root it was given; a
/// single path handed over for comparison has nowhere to escape to, and a
/// checkout reached through a symlink, or one whose `.git` is itself a symlink
/// to a relocated Git store, is somewhere a user can work. Only the presence of
/// the `.git` entry matters, not what kind of entry it is.
///
/// This says nothing about which repository the checkout belongs to. It
/// separates a checkout from a Git directory, which is the distinction
/// `--separate-git-dir` and submodule layouts blur, and it is deliberately not
/// a claim that the checkout is a worktree of any particular repository.
pub fn is_checkout_directory(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(path.join(".git")).is_ok()
}

/// Whether a path is a bare repository: a Git directory used as a repository in
/// its own right, holding `HEAD`, `objects`, and `refs` and no checkout.
///
/// `git worktree list` reports a bare repository as one of its own worktree
/// entries, so it arrives alongside the checkouts. It is not somewhere to work,
/// but it is an ordinary part of a bare-clone-plus-worktrees layout rather than
/// a mistake, so a caller can leave it out without complaining about it.
pub fn is_bare_repository(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && ["HEAD", "objects", "refs"]
            .iter()
            .all(|entry| fs::metadata(path.join(entry)).is_ok())
}

fn scan_remote(
    location: &DiscoveryLocation,
    target: &str,
    binaries: &ProcessBinaries,
    context: &ScanContext<'_>,
) {
    let now = (context.clock)();
    if now >= context.deadline {
        let _ = send_scan(
            context,
            DiscoveryMessage::HostFinished {
                generation: context.generation,
                host: location.host.clone(),
                completion: DiscoveryCompletion::TimedOut,
            },
        );
        return;
    }
    let remaining = context.deadline.saturating_duration_since(now);
    let result = remote_spec(
        target,
        &location.roots,
        binaries,
        context.limits,
        context.entry_limit,
    )
    .map_or(BoundedOutput::Error, |spec| {
        run_bounded(&spec, remaining, context.cancelled)
    });
    if context.cancelled.load(Ordering::Acquire) || matches!(result, BoundedOutput::Cancelled) {
        return;
    }
    let (repositories, completion) = match result {
        BoundedOutput::Completed {
            status,
            stdout,
            stdout_truncated: false,
            ..
        } if status.success() => parse_remote(&stdout, &location.roots, context.limits.max_results),
        BoundedOutput::Completed {
            stdout_truncated: true,
            ..
        } => (Vec::new(), DiscoveryCompletion::OutputLimit),
        BoundedOutput::Completed { status, .. } if status.code() == Some(255) => {
            (Vec::new(), DiscoveryCompletion::Unavailable)
        }
        BoundedOutput::TimedOut => (Vec::new(), DiscoveryCompletion::TimedOut),
        BoundedOutput::Error | BoundedOutput::SpawnError(_) | BoundedOutput::Completed { .. } => {
            (Vec::new(), DiscoveryCompletion::Error)
        }
        BoundedOutput::Cancelled => return,
    };
    for path in repositories {
        if context.cancelled.load(Ordering::Acquire) {
            return;
        }
        if !send_scan(
            context,
            DiscoveryMessage::Repository {
                generation: context.generation,
                host: location.host.clone(),
                path,
            },
        ) {
            return;
        }
    }
    let _ = send_scan(
        context,
        DiscoveryMessage::HostFinished {
            generation: context.generation,
            host: location.host.clone(),
            completion,
        },
    );
}

fn remote_spec(
    target: &str,
    roots: &[String],
    binaries: &ProcessBinaries,
    limits: DiscoveryLimits,
    entry_limit: usize,
) -> anyhow::Result<CommandSpec> {
    let target = openssh_target(target)?;
    let mut remote_args = vec![
        "-c".to_owned(),
        REMOTE_SCAN_SCRIPT.to_owned(),
        "tether-discovery".to_owned(),
        limits.max_depth.to_string(),
        limits.max_results.to_string(),
        entry_limit.to_string(),
    ];
    remote_args.extend(roots.iter().cloned());
    let remote_command = CommandSpec::new("/bin/sh", remote_args).posix_command_line()?;
    let mut ssh_args = openssh_connection_args(false);
    if let Some(port) = target.port {
        ssh_args.extend(["-p".to_owned(), port.to_string()]);
    }
    ssh_args.extend(["--".to_owned(), target.destination, remote_command]);
    Ok(CommandSpec::new(binaries.ssh().to_path_buf(), ssh_args))
}

fn parse_remote(
    stdout: &[u8],
    roots: &[String],
    max_results: usize,
) -> (Vec<String>, DiscoveryCompletion) {
    if stdout.is_empty() {
        return (Vec::new(), DiscoveryCompletion::Complete);
    }
    if stdout.last() != Some(&0) {
        return (Vec::new(), DiscoveryCompletion::Malformed);
    }
    let fields = stdout[..stdout.len() - 1]
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    let mut cursor = 0;
    let mut repositories = Vec::new();
    let mut seen = HashSet::new();
    let mut had_error = false;
    while cursor < fields.len() {
        match fields[cursor] {
            b"R" if cursor + 2 < fields.len() => {
                let Ok(index) = std::str::from_utf8(fields[cursor + 1])
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or(())
                else {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                };
                let Some(root) = roots.get(index) else {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                };
                let relative = match std::str::from_utf8(fields[cursor + 2]) {
                    Ok(relative) => relative,
                    Err(_) => {
                        had_error = true;
                        cursor += 3;
                        continue;
                    }
                };
                if !safe_relative(relative) {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                }
                let path = if relative.is_empty() {
                    root.clone()
                } else {
                    format!("{}/{relative}", root.trim_end_matches('/'))
                };
                if seen.insert(path.clone()) {
                    if repositories.len() >= max_results {
                        return (repositories, DiscoveryCompletion::ResultsLimit);
                    }
                    repositories.push(path);
                }
                cursor += 3;
            }
            b"E" if cursor + 1 < fields.len() => {
                let valid_index = std::str::from_utf8(fields[cursor + 1])
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .is_some_and(|index| index < roots.len());
                if !valid_index {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                }
                had_error = true;
                cursor += 2;
            }
            b"L" => {
                if cursor + 1 != fields.len() {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                }
                return (repositories, DiscoveryCompletion::ResultsLimit);
            }
            b"N" => {
                if cursor + 1 != fields.len() {
                    return (Vec::new(), DiscoveryCompletion::Malformed);
                }
                return (repositories, DiscoveryCompletion::EntriesLimit);
            }
            _ => return (Vec::new(), DiscoveryCompletion::Malformed),
        }
    }
    (
        repositories,
        if had_error {
            DiscoveryCompletion::Error
        } else {
            DiscoveryCompletion::Complete
        },
    )
}

fn safe_relative(path: &str) -> bool {
    path.is_empty()
        || Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_work_limits_have_internal_ceilings() {
        let limits = DiscoveryLimits {
            max_depth: 1,
            max_entries: usize::MAX,
            max_results: usize::MAX,
            timeout: Duration::from_secs(1),
            workers: usize::MAX,
        };
        let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), limits);
        assert_eq!(service.limits.workers, MAX_DISCOVERY_WORKERS);
        assert_eq!(service.limits.max_entries, MAX_DISCOVERY_ENTRIES);
        assert_eq!(service.limits.max_results, MAX_DISCOVERY_RESULTS);
    }

    #[test]
    fn large_directory_candidates_retain_only_the_lexically_first_entry_budget() {
        let mut children = BinaryHeap::new();
        for index in (0..10_000).rev() {
            retain_bounded_child(&mut children, PathBuf::from(format!("{index:05}")), 3);
        }
        let mut children = children.into_vec();
        children.sort();
        assert_eq!(
            children,
            [
                PathBuf::from("00000"),
                PathBuf::from("00001"),
                PathBuf::from("00002"),
            ]
        );
    }

    #[test]
    fn expired_local_scan_stops_before_touching_the_filesystem() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let cancelled = AtomicBool::new(false);
        let entries = AtomicUsize::new(0);
        let now = Instant::now();
        let clock: DiscoveryClock = Arc::new(move || now);
        let context = ScanContext {
            generation: 1,
            sender: &sender,
            cancelled: &cancelled,
            entries: &entries,
            entry_limit: 100,
            limits: DiscoveryLimits {
                max_depth: 4,
                max_entries: 100,
                max_results: 10,
                timeout: Duration::ZERO,
                workers: 1,
            },
            deadline: now,
            clock: &clock,
        };
        let mut scan = LocalScan {
            context: &context,
            host: "local",
            results: 0,
            seen: HashSet::new(),
            completion: DiscoveryCompletion::Complete,
        };

        scan.visit(
            Path::new("/path-that-must-not-be-read"),
            Path::new("/path-that-must-not-be-read"),
            Path::new("/path-that-must-not-be-read"),
            0,
        )
        .unwrap();
        assert_eq!(entries.load(Ordering::Acquire), 0);
        assert_eq!(scan.completion, DiscoveryCompletion::TimedOut);
    }

    #[test]
    fn malformed_remote_records_fail_closed_without_a_process_harness() {
        let roots = vec!["/safe".to_owned()];
        let cases: [&[u8]; 4] = [
            b"R\x000\x00../escape\x00",
            b"R\x000\x00/etc\x00",
            b"R\x001\x00repo\x00",
            b"R\x000\x00repo",
        ];

        for output in cases {
            assert_eq!(
                parse_remote(output, &roots, 10),
                (Vec::new(), DiscoveryCompletion::Malformed)
            );
        }
    }
}
