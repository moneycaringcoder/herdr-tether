use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::mpsc::TryRecvError,
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    backend::ProcessBinaries,
    config::Config,
    discovery::{
        DiscoveryCompletion, DiscoveryLimits, DiscoveryLocation, DiscoveryMessage,
        DiscoveryRequest, DiscoveryRun, DiscoveryService,
    },
    model::SessionId,
    state::{SessionStatus, State},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, StatusHost, StatusMessage,
        StatusRequest, StatusRun, StatusService, WorkloadStatus,
    },
    tui::PickerOptions,
};

const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
const STATUS_WORKERS: usize = 4;
const SHUTDOWN_MARGIN: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub completion: Completion,
    pub hosts: Vec<SnapshotHost>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Complete,
    Partial,
}

#[derive(Debug, Serialize)]
pub struct SnapshotHost {
    pub name: String,
    pub origin: HostOrigin,
    pub target: Option<String>,
    pub roots: Vec<String>,
    pub repositories: Vec<String>,
    pub discovery: TypedStatus,
    pub root_errors: Vec<String>,
    pub reachability: TypedStatus,
    pub owned_sessions: Vec<OwnedSession>,
    pub external_catalog: ExternalCatalog,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostOrigin {
    Builtin,
    Configured,
    SshConfig,
    State,
}

#[derive(Debug, Serialize)]
pub struct TypedStatus {
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct OwnedSession {
    pub id: String,
    pub directory: String,
    pub preset: Option<String>,
    pub metadata_status: String,
    pub workload_status: String,
    pub attached: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ExternalCatalog {
    pub status: String,
    pub sessions: Vec<ExternalCatalogSession>,
    pub hidden_reserved: usize,
    pub hidden_unsafe: usize,
}

#[derive(Debug, Serialize)]
pub struct ExternalCatalogSession {
    pub name: String,
    pub attached: u32,
}

#[derive(Default)]
struct HostResults {
    repositories: Vec<String>,
    root_errors: Vec<String>,
    discovery: Option<DiscoveryCompletion>,
    reachability: Option<HostReachability>,
    workloads: HashMap<SessionId, WorkloadStatus>,
    catalog: Option<(ExternalCatalogStatus, Vec<ExternalSession>, usize, usize)>,
}

pub fn collect(
    config: &Config,
    state: &State,
    aliases: &[String],
    home: &str,
    binaries: ProcessBinaries,
) -> Snapshot {
    let configured_names: HashSet<&str> =
        config.hosts.iter().map(|host| host.name.as_str()).collect();
    let mut picker_config = config.clone();
    picker_config.append_alias_hosts(aliases);
    let options = PickerOptions::from_config_state(&picker_config, state, home, true);
    let effective_keys: HashSet<(String, String)> = options
        .hosts
        .iter()
        .map(|host| {
            (
                host.name.clone(),
                effective_target(host.target.as_deref()).to_owned(),
            )
        })
        .collect();
    let workload_hosts: HashMap<SessionId, String> = state
        .sessions
        .iter()
        .filter(|record| {
            record.status == SessionStatus::Active
                && effective_keys.contains(&(record.host.clone(), record.target.clone()))
        })
        .map(|record| (record.id, record.host.clone()))
        .collect();
    let generation = 1;
    let status_hosts = options
        .hosts
        .iter()
        .map(|host| StatusHost {
            name: host.name.clone(),
            target: host.target.clone(),
            workloads: state
                .sessions
                .iter()
                .filter(|record| {
                    record.host == host.name
                        && record.target == effective_target(host.target.as_deref())
                        && record.status == SessionStatus::Active
                })
                .map(|record| record.id)
                .collect(),
        })
        .collect();
    let locations = options
        .hosts
        .iter()
        .map(|host| DiscoveryLocation {
            host: host.name.clone(),
            target: host.target.clone(),
            roots: host.scan_roots.clone(),
        })
        .collect();

    let status_service = StatusService::new(binaries.clone(), STATUS_TIMEOUT, STATUS_WORKERS);
    let discovery_timeout = Duration::from_secs(config.discovery.timeout_seconds);
    let discovery_workers = config.discovery.workers.max(1);
    let discovery_service = DiscoveryService::new(
        binaries,
        DiscoveryLimits {
            max_depth: config.discovery.max_depth,
            max_entries: config.discovery.max_entries,
            max_results: config.discovery.max_results,
            timeout: discovery_timeout,
            workers: discovery_workers,
        },
    );
    let status_run = status_service.start(StatusRequest {
        generation,
        hosts: status_hosts,
    });
    let discovery_run = discovery_service.start(DiscoveryRequest {
        generation,
        locations,
    });

    let host_count = options.hosts.len();
    let status_waves = host_count.div_ceil(STATUS_WORKERS.max(1));
    let discovery_waves = host_count.div_ceil(discovery_workers);
    let status_budget = STATUS_TIMEOUT.saturating_mul(status_waves as u32);
    let discovery_budget = discovery_timeout.saturating_mul(discovery_waves as u32);
    let deadline = Instant::now() + status_budget.max(discovery_budget);
    let mut results: HashMap<String, HostResults> = options
        .hosts
        .iter()
        .map(|host| (host.name.clone(), HostResults::default()))
        .collect();
    let mut status_finished = false;
    let mut discovery_finished = false;
    let mut status_closed = false;
    let mut discovery_closed = false;
    let mut broken_stream = false;

    while !((status_finished || status_closed) && (discovery_finished || discovery_closed))
        && Instant::now() < deadline
    {
        let progressed = drain_status(
            generation,
            &status_run,
            &workload_hosts,
            &mut results,
            &mut status_finished,
            &mut status_closed,
            &mut broken_stream,
        ) | drain_discovery(
            generation,
            &discovery_run,
            &mut results,
            &mut discovery_finished,
            &mut discovery_closed,
            &mut broken_stream,
        );
        if !progressed {
            thread::sleep(POLL_INTERVAL);
        }
    }

    let cancelled = !(status_finished && discovery_finished);
    if cancelled {
        status_run.cancel();
        discovery_run.cancel();
        let shutdown_deadline = Instant::now() + SHUTDOWN_MARGIN;
        while !((status_finished || status_closed) && (discovery_finished || discovery_closed))
            && Instant::now() < shutdown_deadline
        {
            let progressed = drain_status(
                generation,
                &status_run,
                &workload_hosts,
                &mut results,
                &mut status_finished,
                &mut status_closed,
                &mut broken_stream,
            ) | drain_discovery(
                generation,
                &discovery_run,
                &mut results,
                &mut discovery_finished,
                &mut discovery_closed,
                &mut broken_stream,
            );
            if !progressed {
                thread::sleep(POLL_INTERVAL);
            }
        }
    }

    let mut completion = if cancelled || broken_stream || !status_finished || !discovery_finished {
        Completion::Partial
    } else {
        Completion::Complete
    };
    let mut hosts: Vec<_> = options
        .hosts
        .into_iter()
        .map(|host| {
            let result = results.remove(&host.name).unwrap_or_default();
            let retained_target = effective_target(host.target.as_deref());
            let expected_active: HashSet<_> = state
                .sessions
                .iter()
                .filter(|record| {
                    record.host == host.name
                        && record.target == retained_target
                        && record.status == SessionStatus::Active
                })
                .map(|record| record.id)
                .collect();
            let complete = result.discovery == Some(DiscoveryCompletion::Complete)
                && result.reachability == Some(HostReachability::Reachable)
                && matches!(
                    result.catalog,
                    Some((ExternalCatalogStatus::Available, _, _, _))
                )
                && expected_active
                    .iter()
                    .all(|id| result.workloads.contains_key(id));
            if !complete {
                completion = Completion::Partial;
            }

            let mut repositories = result.repositories;
            repositories.sort();
            repositories.dedup();
            let mut root_errors = result.root_errors;
            root_errors.sort();
            root_errors.dedup();
            let mut owned_sessions: Vec<_> = state
                .sessions
                .iter()
                .filter(|record| record.host == host.name && record.target == retained_target)
                .map(|record| owned_session(record, result.workloads.get(&record.id).copied()))
                .collect();
            owned_sessions.sort_by(|a, b| a.id.cmp(&b.id));
            let catalog_collected = result.catalog.is_some();
            let (catalog_status, external, hidden_reserved, hidden_unsafe) = result
                .catalog
                .unwrap_or((ExternalCatalogStatus::Error, Vec::new(), 0, 0));
            let mut sessions: Vec<_> = external
                .into_iter()
                .map(|session| ExternalCatalogSession {
                    name: session.name.to_string(),
                    attached: session.attached,
                })
                .collect();
            sessions.sort_by(|a, b| a.name.cmp(&b.name));
            SnapshotHost {
                origin: if host.name == "local" {
                    HostOrigin::Builtin
                } else if configured_names.contains(host.name.as_str()) {
                    HostOrigin::Configured
                } else {
                    HostOrigin::SshConfig
                },
                name: host.name,
                target: host.target,
                roots: host.scan_roots,
                repositories,
                discovery: TypedStatus {
                    status: result
                        .discovery
                        .map(discovery_name)
                        .unwrap_or("not_collected")
                        .to_owned(),
                },
                root_errors,
                reachability: TypedStatus {
                    status: result
                        .reachability
                        .map(reachability_name)
                        .unwrap_or("not_collected")
                        .to_owned(),
                },
                owned_sessions,
                external_catalog: ExternalCatalog {
                    status: if catalog_collected {
                        catalog_name(catalog_status)
                    } else {
                        "not_collected"
                    }
                    .to_owned(),
                    sessions,
                    hidden_reserved,
                    hidden_unsafe,
                },
            }
        })
        .collect();

    let mut state_groups: BTreeMap<(String, String), Vec<_>> = BTreeMap::new();
    for record in &state.sessions {
        let key = (record.host.clone(), record.target.clone());
        if !effective_keys.contains(&key) {
            state_groups.entry(key).or_default().push(record);
        }
    }
    if !state_groups.is_empty() {
        completion = Completion::Partial;
    }
    for ((name, target), records) in state_groups {
        let mut owned_sessions: Vec<_> = records
            .into_iter()
            .map(|record| owned_session(record, None))
            .collect();
        owned_sessions.sort_by(|left, right| left.id.cmp(&right.id));
        hosts.push(SnapshotHost {
            name,
            origin: HostOrigin::State,
            target: Some(target),
            roots: Vec::new(),
            repositories: Vec::new(),
            discovery: TypedStatus {
                status: "not_collected".to_owned(),
            },
            root_errors: Vec::new(),
            reachability: TypedStatus {
                status: "not_collected".to_owned(),
            },
            owned_sessions,
            external_catalog: ExternalCatalog {
                status: "not_collected".to_owned(),
                sessions: Vec::new(),
                hidden_reserved: 0,
                hidden_unsafe: 0,
            },
        });
    }
    Snapshot {
        schema_version: 1,
        completion,
        hosts,
    }
}

fn effective_target(target: Option<&str>) -> &str {
    target.unwrap_or("local")
}

fn owned_session(
    record: &crate::state::SessionRecord,
    workload: Option<WorkloadStatus>,
) -> OwnedSession {
    let (workload_status, attached) = match (record.status, workload) {
        (SessionStatus::Closing | SessionStatus::Closed, _) => ("not_checked".to_owned(), None),
        (_, Some(WorkloadStatus::Running { attached })) => ("running".to_owned(), Some(attached)),
        (_, Some(status)) => (workload_name(status).to_owned(), None),
        _ => ("not_collected".to_owned(), None),
    };
    OwnedSession {
        id: record.id.to_string(),
        directory: record.directory.clone(),
        preset: record.preset.clone(),
        metadata_status: metadata_name(record.status).to_owned(),
        workload_status,
        attached,
        created_at: record.created_at,
        last_used_at: record.last_used_at,
        closed_at: record.closed_at,
    }
}

fn drain_status(
    generation: u64,
    run: &StatusRun,
    workload_hosts: &HashMap<SessionId, String>,
    results: &mut HashMap<String, HostResults>,
    finished: &mut bool,
    closed: &mut bool,
    broken: &mut bool,
) -> bool {
    let mut progressed = false;
    loop {
        match run.receiver.try_recv() {
            Ok(message) => {
                progressed = true;
                apply_status(generation, message, workload_hosts, results, finished);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                *closed = true;
                if !*finished {
                    *broken = true;
                }
                break;
            }
        }
    }
    progressed
}

fn drain_discovery(
    generation: u64,
    run: &DiscoveryRun,
    results: &mut HashMap<String, HostResults>,
    finished: &mut bool,
    closed: &mut bool,
    broken: &mut bool,
) -> bool {
    let mut progressed = false;
    loop {
        match run.receiver.try_recv() {
            Ok(message) => {
                progressed = true;
                apply_discovery(generation, message, results, finished);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                *closed = true;
                if !*finished {
                    *broken = true;
                }
                break;
            }
        }
    }
    progressed
}

fn apply_status(
    generation: u64,
    message: StatusMessage,
    workload_hosts: &HashMap<SessionId, String>,
    results: &mut HashMap<String, HostResults>,
    finished: &mut bool,
) {
    if message.generation() != generation {
        return;
    }
    match message {
        StatusMessage::Host { host, status, .. } => {
            if let Some(result) = results.get_mut(&host) {
                result.reachability = Some(status);
            }
        }
        StatusMessage::Workload { id, status, .. } => {
            if let Some(result) = workload_hosts
                .get(&id)
                .and_then(|host| results.get_mut(host))
            {
                result.workloads.insert(id, status);
            }
        }
        StatusMessage::Catalog {
            host,
            status,
            sessions,
            hidden_reserved,
            hidden_unsafe,
            ..
        } => {
            if let Some(result) = results.get_mut(&host) {
                result.catalog = Some((status, sessions, hidden_reserved, hidden_unsafe));
            }
        }
        StatusMessage::Finished { .. } => *finished = true,
    }
}

fn apply_discovery(
    generation: u64,
    message: DiscoveryMessage,
    results: &mut HashMap<String, HostResults>,
    finished: &mut bool,
) {
    if message.generation() != generation {
        return;
    }
    match message {
        DiscoveryMessage::Repository { host, path, .. } => {
            if let Some(result) = results.get_mut(&host) {
                result.repositories.push(path);
            }
        }
        DiscoveryMessage::RootError { host, root, .. } => {
            if let Some(result) = results.get_mut(&host) {
                result.root_errors.push(root);
            }
        }
        DiscoveryMessage::HostFinished {
            host, completion, ..
        } => {
            if let Some(result) = results.get_mut(&host) {
                result.discovery = Some(completion);
            }
        }
        DiscoveryMessage::Finished { .. } => *finished = true,
    }
}

fn metadata_name(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Active => "active",
        SessionStatus::Closing => "closing",
        SessionStatus::Closed => "closed",
    }
}
fn workload_name(status: WorkloadStatus) -> &'static str {
    match status {
        WorkloadStatus::Running { .. } => "running",
        WorkloadStatus::Missing => "missing",
        WorkloadStatus::Unknown => "unknown",
        WorkloadStatus::TimedOut => "timed_out",
        WorkloadStatus::Error => "error",
    }
}
fn discovery_name(status: DiscoveryCompletion) -> &'static str {
    match status {
        DiscoveryCompletion::Complete => "complete",
        DiscoveryCompletion::ResultsLimit => "results_limit",
        DiscoveryCompletion::EntriesLimit => "entries_limit",
        DiscoveryCompletion::TimedOut => "timed_out",
        DiscoveryCompletion::Unavailable => "unavailable",
        DiscoveryCompletion::OutputLimit => "output_limit",
        DiscoveryCompletion::Malformed => "malformed",
        DiscoveryCompletion::Error => "error",
    }
}
fn reachability_name(status: HostReachability) -> &'static str {
    match status {
        HostReachability::Reachable => "reachable",
        HostReachability::Unreachable => "unreachable",
        HostReachability::TimedOut => "timed_out",
        HostReachability::Error => "error",
    }
}
fn catalog_name(status: ExternalCatalogStatus) -> &'static str {
    match status {
        ExternalCatalogStatus::Available => "available",
        ExternalCatalogStatus::Unavailable => "unavailable",
        ExternalCatalogStatus::TimedOut => "timed_out",
        ExternalCatalogStatus::Error => "error",
    }
}
