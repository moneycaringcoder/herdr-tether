use std::{
    collections::{BTreeMap, BinaryHeap, HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    backend::{CommandSpec, ProcessBinaries},
    sshcfg::openssh_target,
    status::{BoundedOutput, run_bounded},
};
const MAX_DISCOVERY_WORKERS: usize = 16;
const MAX_DISCOVERY_ENTRIES: usize = 100_000;
const MAX_DISCOVERY_RESULTS: usize = 4_096;

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
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let completion = Arc::new(AtomicUsize::new(0));
        let jobs = Arc::new(Mutex::new(
            request
                .locations
                .into_iter()
                .enumerate()
                .collect::<VecDeque<_>>(),
        ));
        let location_count = jobs.lock().expect("discovery jobs lock").len();
        let worker_count = self.limits.workers.min(location_count.max(1));
        let (outcome_sender, outcome_receiver) = mpsc::sync_channel(worker_count);
        let entries = Arc::new(AtomicUsize::new(0));
        let started = (self.clock)();
        let deadline = started.checked_add(self.limits.timeout).unwrap_or(started);
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let outcome_sender = outcome_sender.clone();
            let cancelled = Arc::clone(&cancelled);
            let entries = Arc::clone(&entries);
            let binaries = self.binaries.clone();
            let limits = self.limits;
            let clock = Arc::clone(&self.clock);
            let generation = request.generation;
            handles.push(thread::spawn(move || {
                worker_loop(
                    generation,
                    jobs,
                    &outcome_sender,
                    &cancelled,
                    &entries,
                    &binaries,
                    limits,
                    deadline,
                    &clock,
                );
            }));
        }
        drop(outcome_sender);
        let supervisor_cancelled = Arc::clone(&cancelled);
        let supervisor_completion = Arc::clone(&completion);
        let max_results = self.limits.max_results;
        thread::spawn(move || {
            let outcome = publish_outcomes(
                request.generation,
                outcome_receiver,
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
            for handle in handles {
                let _ = handle.join();
            }
            let _ = sender.send(DiscoveryMessage::Finished {
                generation: request.generation,
            });
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

struct LocationOutcome {
    index: usize,
    messages: Vec<DiscoveryMessage>,
}

fn worker_loop(
    generation: u64,
    jobs: Arc<Mutex<VecDeque<(usize, DiscoveryLocation)>>>,
    outcomes: &SyncSender<LocationOutcome>,
    cancelled: &AtomicBool,
    entries: &AtomicUsize,
    binaries: &ProcessBinaries,
    limits: DiscoveryLimits,
    deadline: Instant,
    clock: &DiscoveryClock,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some((index, location)) = jobs.lock().expect("discovery jobs lock").pop_front() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        match &location.target {
            Some(target) => scan_remote(
                generation, &location, target, &sender, cancelled, binaries, limits, deadline,
                clock,
            ),
            None => scan_local(
                generation, &location, &sender, cancelled, entries, limits, deadline, clock,
            ),
        }
        drop(sender);
        let outcome = LocationOutcome {
            index,
            messages: receiver.into_iter().collect(),
        };
        if outcomes.send(outcome).is_err() {
            return;
        }
    }
}

fn publish_outcomes(
    generation: u64,
    outcomes: Receiver<LocationOutcome>,
    sender: &Sender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    max_results: usize,
) -> DiscoveryRunCompletion {
    let mut overall = DiscoveryRunCompletion::Complete;
    let mut pending = BTreeMap::new();
    let mut next = 0;
    let mut published_results = 0;
    for outcome in outcomes {
        pending.insert(outcome.index, outcome.messages);
        while let Some(messages) = pending.remove(&next) {
            let mut exceeded_results = false;
            for message in messages {
                if cancelled.load(Ordering::Acquire) {
                    return DiscoveryRunCompletion::Cancelled;
                }
                match message {
                    DiscoveryMessage::Repository { .. } if published_results >= max_results => {
                        exceeded_results = true;
                    }
                    DiscoveryMessage::Repository { .. } => {
                        published_results += 1;
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
            next += 1;
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
    generation: u64,
    host: &'a str,
    sender: &'a Sender<DiscoveryMessage>,
    cancelled: &'a AtomicBool,
    limits: DiscoveryLimits,
    deadline: Instant,
    entries: &'a AtomicUsize,
    clock: &'a DiscoveryClock,
    results: usize,
    seen: HashSet<PathBuf>,
    completion: DiscoveryCompletion,
}

fn scan_local(
    generation: u64,
    location: &DiscoveryLocation,
    sender: &Sender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    entries: &AtomicUsize,
    limits: DiscoveryLimits,
    deadline: Instant,
    clock: &DiscoveryClock,
) {
    let mut scan = LocalScan {
        generation,
        host: &location.host,
        sender,
        cancelled,
        limits,
        deadline,
        entries,
        clock,
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
        if result.is_err() && !scan.cancelled.load(Ordering::Acquire) {
            scan.completion = DiscoveryCompletion::Error;
            let _ = scan.sender.send(DiscoveryMessage::RootError {
                generation,
                host: location.host.clone(),
                root: root.clone(),
            });
        }
    }
    if !cancelled.load(Ordering::Acquire) {
        let _ = sender.send(DiscoveryMessage::HostFinished {
            generation,
            host: location.host.clone(),
            completion: scan.completion,
        });
    }
}

impl LocalScan<'_> {
    fn stopped(&mut self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if self.completion == DiscoveryCompletion::ResultsLimit {
            return true;
        }
        if (self.clock)() >= self.deadline {
            self.completion = DiscoveryCompletion::TimedOut;
            return true;
        }
        if self.entries.load(Ordering::Acquire) >= self.limits.max_entries {
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
            .entries
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |entries| {
                (entries < self.limits.max_entries).then_some(entries + 1)
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
                if self.results >= self.limits.max_results {
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
                let _ = self.sender.send(DiscoveryMessage::Repository {
                    generation: self.generation,
                    host: self.host.to_owned(),
                    path: path.to_owned(),
                });
            }
            return Ok(());
        }
        if depth >= self.limits.max_depth {
            return Ok(());
        }
        let remaining_entries = self
            .limits
            .max_entries
            .saturating_sub(self.entries.load(Ordering::Acquire));
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

fn scan_remote(
    generation: u64,
    location: &DiscoveryLocation,
    target: &str,
    sender: &Sender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    limits: DiscoveryLimits,
    deadline: Instant,
    clock: &DiscoveryClock,
) {
    let now = (clock)();
    if now >= deadline {
        let _ = sender.send(DiscoveryMessage::HostFinished {
            generation,
            host: location.host.clone(),
            completion: DiscoveryCompletion::TimedOut,
        });
        return;
    }
    let remaining = deadline.saturating_duration_since(now);
    let result = remote_spec(target, &location.roots, binaries, limits)
        .map_or(BoundedOutput::Error, |spec| {
            run_bounded(&spec, remaining, cancelled)
        });
    if cancelled.load(Ordering::Acquire) || matches!(result, BoundedOutput::Cancelled) {
        return;
    }
    let (repositories, completion) = match result {
        BoundedOutput::Completed {
            status,
            stdout,
            stdout_truncated: false,
            ..
        } if status.success() => parse_remote(&stdout, &location.roots, limits.max_results),
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
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if sender
            .send(DiscoveryMessage::Repository {
                generation,
                host: location.host.clone(),
                path,
            })
            .is_err()
        {
            return;
        }
    }
    let _ = sender.send(DiscoveryMessage::HostFinished {
        generation,
        host: location.host.clone(),
        completion,
    });
}

fn remote_spec(
    target: &str,
    roots: &[String],
    binaries: &ProcessBinaries,
    limits: DiscoveryLimits,
) -> anyhow::Result<CommandSpec> {
    let target = openssh_target(target)?;
    let mut remote_args = vec![
        "-c".to_owned(),
        REMOTE_SCAN_SCRIPT.to_owned(),
        "tether-discovery".to_owned(),
        limits.max_depth.to_string(),
        limits.max_results.to_string(),
        limits.max_entries.to_string(),
    ];
    remote_args.extend(roots.iter().cloned());
    let remote_command = CommandSpec::new("/bin/sh", remote_args).posix_command_line()?;
    let mut ssh_args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "ServerAliveInterval=15".to_owned(),
        "-o".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
    ];
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
        let (sender, _receiver) = mpsc::channel();
        let cancelled = AtomicBool::new(false);
        let mut scan = LocalScan {
            generation: 1,
            host: "local",
            sender: &sender,
            cancelled: &cancelled,
            limits: DiscoveryLimits {
                max_depth: 4,
                max_entries: 100,
                max_results: 10,
                timeout: Duration::ZERO,
                workers: 1,
            },
            started: Instant::now(),
            entries: 0,
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
        assert_eq!(scan.entries, 0);
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
