use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    backend::{CommandSpec, ProcessBinaries},
    sshcfg::validate_ssh_target,
    status::{BoundedOutput, run_bounded},
};

const REMOTE_SCAN_SCRIPT: &str = r#"max_depth=$1
result_limit=$2
entry_limit=$3
shift 3
LC_ALL=C
export LC_ALL
entries=0
results=0

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
  for child in "$1"/* "$1"/.[!.]* "$1"/..?*; do
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
  done
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

#[derive(Clone, Debug)]
pub struct DiscoveryService {
    binaries: ProcessBinaries,
    limits: DiscoveryLimits,
}

impl DiscoveryService {
    pub fn new(binaries: ProcessBinaries, limits: DiscoveryLimits) -> Self {
        Self { binaries, limits }
    }

    pub fn start(&self, request: DiscoveryRequest) -> DiscoveryRun {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let jobs = Arc::new(Mutex::new(VecDeque::from(request.locations)));
        let worker_count = self
            .limits
            .workers
            .max(1)
            .min(jobs.lock().expect("discovery jobs lock").len().max(1));
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let sender = sender.clone();
            let cancelled = Arc::clone(&cancelled);
            let binaries = self.binaries.clone();
            let limits = self.limits;
            let generation = request.generation;
            handles.push(thread::spawn(move || {
                worker_loop(generation, jobs, &sender, &cancelled, &binaries, limits);
            }));
        }
        thread::spawn(move || {
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
        }
    }
}

pub struct DiscoveryRun {
    pub receiver: Receiver<DiscoveryMessage>,
    cancelled: Arc<AtomicBool>,
}

impl DiscoveryRun {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for DiscoveryRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn worker_loop(
    generation: u64,
    jobs: Arc<Mutex<VecDeque<DiscoveryLocation>>>,
    sender: &Sender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    binaries: &ProcessBinaries,
    limits: DiscoveryLimits,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(location) = jobs.lock().expect("discovery jobs lock").pop_front() else {
            return;
        };
        match &location.target {
            Some(target) => scan_remote(
                generation, &location, target, sender, cancelled, binaries, limits,
            ),
            None => scan_local(generation, &location, sender, cancelled, limits),
        }
    }
}

struct LocalScan<'a> {
    generation: u64,
    host: &'a str,
    sender: &'a Sender<DiscoveryMessage>,
    cancelled: &'a AtomicBool,
    limits: DiscoveryLimits,
    started: Instant,
    entries: usize,
    results: usize,
    seen: HashSet<PathBuf>,
    completion: DiscoveryCompletion,
}

fn scan_local(
    generation: u64,
    location: &DiscoveryLocation,
    sender: &Sender<DiscoveryMessage>,
    cancelled: &AtomicBool,
    limits: DiscoveryLimits,
) {
    let mut scan = LocalScan {
        generation,
        host: &location.host,
        sender,
        cancelled,
        limits,
        started: Instant::now(),
        entries: 0,
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
        if self.started.elapsed() >= self.limits.timeout {
            self.completion = DiscoveryCompletion::TimedOut;
            return true;
        }
        if self.entries >= self.limits.max_entries {
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
        self.entries += 1;
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
        let mut children = Vec::new();
        for entry in fs::read_dir(path)? {
            match entry {
                Ok(entry) => children.push(entry.path()),
                Err(error) if ignorable_scan_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
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
) {
    let result = remote_spec(target, &location.roots, binaries, limits)
        .map_or(BoundedOutput::Error, |spec| {
            run_bounded(&spec, limits.timeout, cancelled)
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
    validate_ssh_target(target)?;
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
    Ok(CommandSpec::new(
        binaries.ssh().to_path_buf(),
        vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            "--".to_owned(),
            target.to_owned(),
            remote_command,
        ],
    ))
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
