use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use herdr_tether::{
    backend::ProcessBinaries,
    model::{ExternalSessionName, SessionId},
    status::{
        ExternalCatalogStatus, ExternalSession, HealthStatus, HostReachability,
        MAX_STATUS_WORKLOADS, ResourceReport, StatusHost, StatusMessage, StatusRequest,
        StatusRequestError, StatusService, StatusWorkload, WorkloadStatus,
    },
};
use tempfile::tempdir;

/// A workload to probe for liveness, with no health command configured.
fn probe(id: SessionId) -> StatusWorkload {
    StatusWorkload {
        id,
        directory: "/srv/app".to_owned(),
        health_command: None,
    }
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_fake_ssh(path: &Path, fast_id: SessionId) {
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" fast "*) printf '%s:2:1:1:0::\n' '{fast_id}' ;;
  *" slow "*) sleep 5 ;;
  *) exit 99 ;;
esac
"#
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn id(value: &str) -> SessionId {
    value.parse().unwrap()
}

#[test]
fn fast_host_publishes_before_slow_host_times_out() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let fast_id = id("tether-0197f198000070008000000000000001");
    let slow_id = id("tether-0197f198000070008000000000000002");
    write_fake_ssh(&ssh, fast_id);

    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_millis(500),
        2,
    );
    let started = Instant::now();
    let run = service
        .try_start(StatusRequest {
            generation: 7,
            resources: false,
            hosts: vec![
                StatusHost {
                    name: "slow".into(),
                    target: Some("slow".into()),
                    workloads: vec![probe(slow_id)],
                },
                StatusHost {
                    name: "fast".into(),
                    target: Some("fast".into()),
                    workloads: vec![probe(fast_id)],
                },
            ],
        })
        .unwrap();

    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { generation: 7 });
        messages.push(message);
        if finished {
            break;
        }
    }

    let fast = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                StatusMessage::Workload {
                    generation: 7,
                    id,
                    status: WorkloadStatus::Running { attached: 2 },
                    ..
                } if *id == fast_id
            )
        })
        .expect("fast workload result");
    let slow = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                StatusMessage::Host {
                    generation: 7,
                    host,
                    status: HostReachability::TimedOut,
                    ..
                } if host == "slow"
            )
        })
        .expect("slow timeout result");

    assert!(fast < slow, "fast host must publish before slow timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn an_ordinary_refresh_sees_a_workload_that_ended_on_its_own() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let clean = id("tether-0197f198000070008000000000000001");
    let failed = id("tether-0197f198000070008000000000000002");
    let unreadable = id("tether-0197f198000070008000000000000003");
    let signalled = id("tether-0197f198000070008000000000000005");
    let alive = id("tether-0197f198000070008000000000000004");
    // `remain-on-exit` keeps a session listed after its command exits, so every
    // one of these is still present. Only the pane fields tell them apart.
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s:0:1:1:1:0:\\n%s:0:1:1:1:2:\\n%s:0:1:1:1::\\n%s:0:1:1:1::9\\n%s:1:1:1:0::\\n' '{clean}' '{failed}' '{unreadable}' '{signalled}' '{alive}'\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 3,
            resources: false,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![
                    probe(clean),
                    probe(failed),
                    probe(unreadable),
                    probe(signalled),
                    probe(alive),
                ],
            }],
        })
        .unwrap();
    let mut statuses = std::collections::HashMap::new();
    loop {
        match run.receiver.recv_timeout(Duration::from_secs(15)).unwrap() {
            StatusMessage::Workload { id, status, .. } => {
                statuses.insert(id, status);
            }
            StatusMessage::Finished { .. } => break,
            _ => {}
        }
    }

    assert_eq!(
        statuses.get(&clean),
        Some(&WorkloadStatus::Ended {
            exit_status: Some(0)
        }),
        "a workload whose command exited is not running: {statuses:?}"
    );
    assert_eq!(
        statuses.get(&failed),
        Some(&WorkloadStatus::Ended {
            exit_status: Some(2)
        })
    );
    assert_eq!(
        statuses.get(&unreadable),
        Some(&WorkloadStatus::Ended { exit_status: None }),
        "an end whose status tmux could not report is still an end"
    );
    assert_eq!(
        statuses.get(&signalled),
        Some(&WorkloadStatus::Ended {
            exit_status: Some(137)
        }),
        "a workload the kernel killed is a failure, not a clean end: {statuses:?}"
    );
    assert_eq!(
        statuses.get(&alive),
        Some(&WorkloadStatus::Running { attached: 1 }),
        "a live pane is untouched by this"
    );
}

#[test]
fn a_pane_someone_added_is_not_the_workload_ending() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let split = id("tether-0197f198000070008000000000000001");
    let windowed = id("tether-0197f198000070008000000000000002");
    let alone = id("tether-0197f198000070008000000000000003");
    // `#{pane_dead}` on a session describes its active pane. Someone attached to a
    // workload can split it and let the split exit, which is what the first two
    // lines are: a dead active pane in a session that still holds live work.
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s:1:1:2:1:7:\\n%s:1:2:1:1:7:\\n%s:0:1:1:1:7:\\n' '{split}' '{windowed}' '{alone}'\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 4,
            resources: false,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![probe(split), probe(windowed), probe(alone)],
            }],
        })
        .unwrap();
    let mut statuses = std::collections::HashMap::new();
    loop {
        match run.receiver.recv_timeout(Duration::from_secs(15)).unwrap() {
            StatusMessage::Workload { id, status, .. } => {
                statuses.insert(id, status);
            }
            StatusMessage::Finished { .. } => break,
            _ => {}
        }
    }

    assert_eq!(
        statuses.get(&split),
        Some(&WorkloadStatus::Running { attached: 1 }),
        "a dead split is not the workload ending: {statuses:?}"
    );
    assert_eq!(
        statuses.get(&windowed),
        Some(&WorkloadStatus::Running { attached: 1 }),
        "nor is a dead pane in one of several windows: {statuses:?}"
    );
    assert_eq!(
        statuses.get(&alone),
        Some(&WorkloadStatus::Ended {
            exit_status: Some(7)
        }),
        "the workload's own pane, alone as Tether launched it, is the one that counts"
    );
}

#[test]
fn catalog_publishes_only_safe_non_tether_sessions() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let owned = id("tether-0197f198000070008000000000000001");
    let collision = id("tether-0197f198000070008000000000000002");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s:0:1:1:0::\\n%s:2:1:1:0::\\n%s:0:1:1:1:7:\\ntether-malformed:0:1:1:0::\\n' 'work box' '{owned}' '{collision}'\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 8,
            resources: false,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![probe(owned)],
            }],
        })
        .unwrap();
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { generation: 8 });
        messages.push(message);
        if finished {
            break;
        }
    }

    assert!(
        messages.iter().any(|message| matches!(
            message,
            StatusMessage::Catalog {
                host,
                status: ExternalCatalogStatus::Available,
                sessions,
                hidden_reserved: 2,
                ..
            } if host == "dev"
                && sessions == &[ExternalSession {
                    name: "work box".parse::<ExternalSessionName>().unwrap(),
                    attached: 0,
                }]
        )),
        "unexpected status messages: {messages:?}"
    );
    assert!(
        messages.iter().any(|message| matches!(
            message,
            StatusMessage::Workload {
                id,
                status: WorkloadStatus::Running { attached: 2 },
                ..
            } if *id == owned
        )),
        "unexpected status messages: {messages:?}"
    );
}

#[test]
fn catalog_fails_closed_on_duplicates_and_skips_unsafe_names() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
target=
for argument do
  case "$argument" in duplicate|unsafe) target=$argument ;; esac
done
case "$target" in
  duplicate) printf 'work:0:1:1:0::\nwork:1:1:1:0::\n' ;;
  unsafe) printf 'good:0:1:1:1:7:\nbad:name:0:1:1:0::\n' ;;
  *) exit 99 ;;
esac
"#,
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let duplicate_id = id("tether-0197f198000070008000000000000001");
    let unsafe_id = id("tether-0197f198000070008000000000000002");
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        2,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 10,
            resources: false,
            hosts: vec![
                StatusHost {
                    name: "duplicate".into(),
                    target: Some("duplicate".into()),
                    workloads: vec![probe(duplicate_id)],
                },
                StatusHost {
                    name: "unsafe".into(),
                    target: Some("unsafe".into()),
                    workloads: vec![probe(unsafe_id)],
                },
            ],
        })
        .unwrap();
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { generation: 10 });
        messages.push(message);
        if finished {
            break;
        }
    }

    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Catalog {
            host,
            status: ExternalCatalogStatus::Error,
            sessions,
            ..
        } if host == "duplicate" && sessions.is_empty()
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Workload {
            id,
            status: WorkloadStatus::Unknown,
            ..
        } if *id == duplicate_id
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Catalog {
            host,
            status: ExternalCatalogStatus::Available,
            sessions,
            hidden_unsafe: 1,
            ..
        } if host == "unsafe"
            && sessions == &[ExternalSession {
                name: "good".parse().unwrap(),
                attached: 0,
            }]
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Workload {
            id,
            status: WorkloadStatus::Missing,
            ..
        } if *id == unsafe_id
    )));
}

#[test]
fn cancelled_generation_does_not_publish_late_results() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let workload = id("tether-0197f198000070008000000000000001");
    write_fake_ssh(&ssh, workload);
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(3),
        2,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 1,
            resources: false,
            hosts: vec![StatusHost {
                name: "slow".into(),
                target: Some("slow".into()),
                workloads: vec![probe(workload)],
            }],
        })
        .unwrap();

    run.cancel();
    let started = Instant::now();
    while let Ok(message) = run.receiver.recv_timeout(Duration::from_millis(100)) {
        assert!(matches!(message, StatusMessage::Finished { generation: 1 }));
    }
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn probe_failures_do_not_masquerade_as_live_or_missing() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
target=
for argument do
  case "$argument" in
    offline|empty|malformed) target=$argument ;;
  esac
done
case "$target" in
  offline) exit 255 ;;
  empty) exit 1 ;;
  malformed) printf 'tether-not-an-id:0:1:1:0::\n' ;;
  *) exit 99 ;;
esac
"#,
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let offline_id = id("tether-0197f198000070008000000000000001");
    let empty_id = id("tether-0197f198000070008000000000000002");
    let malformed_id = id("tether-0197f198000070008000000000000003");
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        3,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 9,
            resources: false,
            hosts: vec![
                StatusHost {
                    name: "offline".into(),
                    target: Some("offline".into()),
                    workloads: vec![probe(offline_id)],
                },
                StatusHost {
                    name: "empty".into(),
                    target: Some("empty".into()),
                    workloads: vec![probe(empty_id)],
                },
                StatusHost {
                    name: "malformed".into(),
                    target: Some("malformed".into()),
                    workloads: vec![probe(malformed_id)],
                },
            ],
        })
        .unwrap();
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { generation: 9 });
        messages.push(message);
        if finished {
            break;
        }
    }

    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Host {
            host,
            status: HostReachability::Unreachable,
            ..
        } if host == "offline"
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Workload {
            id,
            status: WorkloadStatus::Unknown,
            ..
        } if *id == offline_id
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Host {
            host,
            status: HostReachability::Reachable,
            ..
        } if host == "empty"
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Workload {
            id,
            status: WorkloadStatus::Missing,
            ..
        } if *id == empty_id
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Workload {
            id,
            status: WorkloadStatus::Missing,
            ..
        } if *id == malformed_id
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        StatusMessage::Catalog {
            host,
            status: ExternalCatalogStatus::Available,
            hidden_reserved: 1,
            ..
        } if host == "malformed"
    )));
}

#[test]
fn local_spawn_error_includes_actionable_tool_locations() {
    let temp = tempdir().unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(
            temp.path().join("missing-ssh"),
            temp.path().join("missing-tmux"),
        ),
        Duration::from_millis(100),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 9,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: Vec::new(),
            }],
        })
        .unwrap();

    let message = run.receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    let StatusMessage::Host {
        status: HostReachability::Error,
        detail: Some(detail),
        ..
    } = message
    else {
        panic!("expected actionable local host error");
    };
    assert!(detail.contains("could not start tmux"));
    assert!(detail.contains("/opt/homebrew/bin"));
    assert!(detail.contains("/usr/local/bin"));
}

#[cfg(unix)]
#[test]
fn a_configured_health_command_is_probed_after_liveness() {
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    // A `tmux` that lists nothing, so the workload reads as missing while the
    // health command still runs: the two answers are independent.
    fs::write(&tmux, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::from_secs(10),
        1,
    );
    let serving = id("tether-0197f198000070008000000000000001");
    let failing = id("tether-0197f198000070008000000000000002");
    let run = service
        .try_start(StatusRequest {
            generation: 11,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: vec![
                    StatusWorkload {
                        id: serving,
                        directory: "/".to_owned(),
                        health_command: Some("exit 0".to_owned()),
                    },
                    StatusWorkload {
                        id: failing,
                        directory: "/".to_owned(),
                        health_command: Some("exit 4".to_owned()),
                    },
                    // No probe configured, so no health result may appear.
                    probe(id("tether-0197f198000070008000000000000003")),
                ],
            }],
        })
        .unwrap();

    let mut health = Vec::new();
    let mut liveness_after_health = 0;
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        match message {
            StatusMessage::Health { id, status, .. } => health.push((id, status)),
            // Liveness must land first: a workload observed as missing is taken
            // off the row before a verdict about it could be drawn.
            StatusMessage::Workload { .. } if !health.is_empty() => liveness_after_health += 1,
            _ => {}
        }
        if finished {
            break;
        }
    }

    assert_eq!(
        liveness_after_health, 0,
        "every liveness result must precede the first health result"
    );
    assert_eq!(
        health,
        vec![
            (serving, HealthStatus::Serving),
            (
                failing,
                HealthStatus::NotServing {
                    exit_status: Some(4)
                }
            ),
        ]
    );
}

#[cfg(unix)]
#[test]
fn an_unreachable_host_reports_unknown_health_without_probing() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    // The status `ssh` reserves for its own transport failures. A probe that
    // never reached the machine must not claim the workload is not serving.
    let marker = temp.path().join("attempts");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf 'x' >> '{}'\nexit 255\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let workload = id("tether-0197f198000070008000000000000001");
    let run = service
        .try_start(StatusRequest {
            generation: 12,
            resources: false,
            hosts: vec![StatusHost {
                name: "offline".into(),
                target: Some("offline".into()),
                workloads: vec![StatusWorkload {
                    id: workload,
                    directory: "/srv/app".to_owned(),
                    health_command: Some("exit 0".to_owned()),
                }],
            }],
        })
        .unwrap();

    let mut health = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        if let StatusMessage::Health { id, status, .. } = message {
            health.push((id, status));
        }
        if finished {
            break;
        }
    }

    assert_eq!(health, vec![(workload, HealthStatus::Unknown)]);
    // The liveness attempt already proved the host cannot answer, so the probe
    // is not retried per workload.
    assert_eq!(
        fs::read(&marker).unwrap().len(),
        1,
        "an unreachable host must be contacted once, not once per probe"
    );
}

#[cfg(unix)]
#[test]
fn the_health_phase_stays_bounded_however_many_workloads_are_probed() {
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    fs::write(&tmux, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::from_secs(1),
        1,
    );
    // Enough probes that a per-probe budget would cost eight seconds, while one
    // shared phase budget costs one however many there are.
    let ids: Vec<_> = (1..=8)
        .map(|index| id(&format!("tether-0197f19800007000800000000000000{index}")))
        .collect();
    let started = Instant::now();
    let run = service
        .try_start(StatusRequest {
            generation: 13,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: ids
                    .iter()
                    .map(|id| StatusWorkload {
                        id: *id,
                        directory: "/".to_owned(),
                        // A probe that never answers, so the phase's own budget
                        // is what has to stop it.
                        health_command: Some("sleep 30".to_owned()),
                    })
                    .collect(),
            }],
        })
        .unwrap();

    let mut health = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(20)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        if let StatusMessage::Health { id, status, .. } = message {
            health.push((id, status));
        }
        if finished {
            break;
        }
    }
    let elapsed = started.elapsed();

    assert_eq!(
        health,
        ids.iter()
            .map(|id| (*id, HealthStatus::Unknown))
            .collect::<Vec<_>>(),
        "a probe that does not fit the phase reports unknown"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the health phase must not grow with workload count: {elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn resource_figures_are_reported_per_workload_and_absence_is_explicit() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let busy = id("tether-0197f198000070008000000000000001");
    let idle = id("tether-0197f198000070008000000000000002");
    // A host that answers all three questions: which sessions exist, which
    // process each pane belongs to, and what the processes are using. The busy
    // workload's own pane is idle while its child burns a core, which is exactly
    // the case a figure taken from the pane alone would miss.
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n\
             *list-sessions*) printf '%s:1:1:1:0::\\n%s:0:1:1:0::\\n' '{busy}' '{idle}' ;;\n\
             *list-panes*) printf '%s:100\\n%s:200\\n' '{busy}' '{idle}' ;;\n\
             *ps*) printf '100 1 00:10 4096\\n101 100 00:00 1048576\\n200 1 00:05 8192\\ntether-sample\\n100 00:10\\n101 00:01\\n200 00:05\\n' ;;\n\
             *) exit 99 ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let absent = id("tether-0197f198000070008000000000000003");
    let run = service
        .try_start(StatusRequest {
            generation: 14,
            resources: true,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![probe(busy), probe(idle), probe(absent)],
            }],
        })
        .unwrap();

    let mut reports = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        if let StatusMessage::Resources { id, report, .. } = message {
            reports.push((id, report));
        }
        if finished {
            break;
        }
    }

    let report = |wanted| {
        reports
            .iter()
            .find(|(id, _)| *id == wanted)
            .map(|(_, report)| *report)
            .expect("every requested workload is reported")
    };
    let ResourceReport::Known(usage) = report(busy) else {
        panic!("the busy workload reports a figure: {reports:?}");
    };
    // The child used a second of processor time while Tether waited a second;
    // the pane's own shell used none. A figure from the pane alone would be zero.
    assert!((usage.cpu_percent - 100.0).abs() < 0.01, "{usage:?}");
    assert_eq!(usage.memory_bytes, (4096 + 1_048_576) * 1024);
    let ResourceReport::Known(usage) = report(idle) else {
        panic!("the idle workload still reports a figure: {reports:?}");
    };
    assert!(
        usage.cpu_percent.abs() < 0.01,
        "an idle workload is idle: {usage:?}"
    );
    // A workload the host said nothing about is unknown, not zero.
    assert_eq!(report(absent), ResourceReport::Unknown);
}

#[cfg(unix)]
#[test]
fn an_unreachable_host_reports_unknown_usage_without_asking() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let marker = temp.path().join("attempts");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf 'x' >> '{}'\nexit 255\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let workload = id("tether-0197f198000070008000000000000001");
    let run = service
        .try_start(StatusRequest {
            generation: 15,
            resources: true,
            hosts: vec![StatusHost {
                name: "offline".into(),
                target: Some("offline".into()),
                workloads: vec![probe(workload)],
            }],
        })
        .unwrap();

    let mut reports = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        if let StatusMessage::Resources { id, report, .. } = message {
            reports.push((id, report));
        }
        if finished {
            break;
        }
    }

    assert_eq!(reports, vec![(workload, ResourceReport::Unknown)]);
    // The liveness attempt already proved the host cannot answer; asking it two
    // more questions per refresh would spend the refresh relearning that.
    assert_eq!(
        fs::read(&marker).unwrap().len(),
        1,
        "an unreachable host is contacted once, not once per phase"
    );
}

#[test]
fn unrepresentable_timeout_does_not_lose_a_spawned_probe() {
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    fs::write(&tmux, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::MAX,
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 10,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: Vec::new(),
            }],
        })
        .unwrap();

    let first = run.receiver.recv_timeout(Duration::from_secs(1)).unwrap();

    assert!(matches!(
        first,
        StatusMessage::Host {
            generation: 10,
            status: HostReachability::Reachable,
            ..
        }
    ));
    assert!(matches!(
        run.receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        StatusMessage::Catalog {
            generation: 10,
            status: ExternalCatalogStatus::Available,
            ..
        }
    ));
    assert!(matches!(
        run.receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        StatusMessage::Finished { generation: 10 }
    ));
}

#[cfg(unix)]
#[test]
fn a_caller_that_does_not_ask_for_usage_does_not_pay_for_it() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let argv = temp.path().join("argv");
    let workload = id("tether-0197f198000070008000000000000001");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s:1:1:1:0::\\n' '{workload}'\n",
            argv.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 16,
            resources: false,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![probe(workload)],
            }],
        })
        .unwrap();

    let mut reports = 0;
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { .. });
        if matches!(message, StatusMessage::Resources { .. }) {
            reports += 1;
        }
        if finished {
            break;
        }
    }

    assert_eq!(
        reports, 0,
        "no figures were asked for, so none are reported"
    );
    // The two extra questions are the cost this flag exists to avoid: a caller
    // that shows no figures must not make the host answer them.
    let invoked = fs::read_to_string(&argv).unwrap();
    assert!(
        !invoked.contains("list-panes") && !invoked.contains("ps -"),
        "unexpected resource commands: {invoked}"
    );
}

#[test]
fn unreachable_remote_reports_typed_context_without_raw_ssh_stderr() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nprintf '\\033[31mssh: connect to host private.example port 22: Connection refused\\033[0m\\n' >&2\nexit 255\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(1),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 11,
            resources: false,
            hosts: vec![StatusHost {
                name: "private-alias".into(),
                target: Some("private.example".into()),
                workloads: Vec::new(),
            }],
        })
        .unwrap();

    let StatusMessage::Host {
        status: HostReachability::Unreachable,
        detail: Some(detail),
        ..
    } = run.receiver.recv_timeout(Duration::from_secs(1)).unwrap()
    else {
        panic!("expected typed unreachable host detail");
    };
    assert_eq!(
        detail,
        "SSH connection was refused; check that the host is online and SSH is running"
    );
    assert!(!detail.contains("private.example"));
    assert!(!detail.contains('\u{1b}'));
}

#[cfg(unix)]
#[test]
fn a_host_with_nothing_running_is_not_asked_what_it_is_using() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let argv = temp.path().join("argv");
    // The host answers, but none of the requested workloads is there, so no row
    // can carry a figure. Asking two more questions would buy nothing.
    fs::write(
        &ssh,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", argv.display()),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        Duration::from_secs(10),
        1,
    );
    let workload = id("tether-0197f198000070008000000000000001");
    let run = service
        .try_start(StatusRequest {
            generation: 17,
            resources: true,
            hosts: vec![StatusHost {
                name: "dev".into(),
                target: Some("dev".into()),
                workloads: vec![probe(workload)],
            }],
        })
        .unwrap();

    while let Ok(message) = run.receiver.recv_timeout(Duration::from_secs(15)) {
        if matches!(message, StatusMessage::Finished { .. }) {
            break;
        }
    }

    let invoked = fs::read_to_string(&argv).unwrap();
    assert!(
        !invoked.contains("list-panes") && !invoked.contains("ps -"),
        "a host with nothing running was asked anyway: {invoked}"
    );
}

#[cfg(unix)]
#[test]
fn successful_probe_does_not_leave_background_descendants_running() {
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let pid_path = temp.path().join("descendant.pid");
    fs::write(
        &tmux,
        format!(
            "#!/bin/sh\nsleep 30 >/dev/null 2>&1 &\nprintf '%s\\n' \"$!\" > '{}'\nexit 1\n",
            pid_path.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::from_secs(1),
        1,
    );
    let run = service
        .try_start(StatusRequest {
            generation: 12,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: Vec::new(),
            }],
        })
        .unwrap();

    while !matches!(
        run.receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        StatusMessage::Finished { generation: 12 }
    ) {}
    let pid = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal 0 performs existence/permission checking without
        // delivering a signal, and `pid` came from the child shell.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        if !exists {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful bounded probe left descendant {pid} running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn status_rejects_workload_cardinality_n_plus_one_before_probe() {
    let temp = tempdir().unwrap();
    let marker = temp.path().join("probed");
    let tmux = temp.path().join("tmux");
    fs::write(
        &tmux,
        format!("#!/bin/sh\nprintf x >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::from_secs(1),
        1,
    );
    let workload = id("tether-0197f198000070008000000000000001");
    let error = service
        .try_start(StatusRequest {
            generation: 20,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: vec![probe(workload); MAX_STATUS_WORKLOADS + 1],
            }],
        })
        .unwrap_err();

    assert_eq!(
        error,
        StatusRequestError::TooManyWorkloads {
            actual: MAX_STATUS_WORKLOADS + 1,
            maximum: MAX_STATUS_WORKLOADS,
        }
    );
    assert!(!marker.exists(), "an invalid request reached the probe");

    let run = service
        .try_start(StatusRequest {
            generation: 21,
            resources: false,
            hosts: vec![StatusHost {
                name: "local".into(),
                target: None,
                workloads: vec![probe(workload); MAX_STATUS_WORKLOADS],
            }],
        })
        .expect("the exact workload boundary must remain valid");
    loop {
        if matches!(
            run.receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            StatusMessage::Finished { generation: 21 }
        ) {
            break;
        }
    }
    assert!(
        marker.exists(),
        "a boundary-valid request did not reach the probe"
    );
}

#[test]
fn duplicate_heavy_status_request_probes_and_reports_each_exact_target_once() {
    let temp = tempdir().unwrap();
    let calls = temp.path().join("calls");
    let tmux = temp.path().join("tmux");
    let first = id("tether-0197f198000070008000000000000001");
    let second = id("tether-0197f198000070008000000000000002");
    fs::write(
        &tmux,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\nprintf '{}: 1 windows (created now) (attached)\\n{}: 1 windows (created now)\\n'\n",
            calls.display(),
            first,
            second,
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new(temp.path().join("ssh"), &tmux),
        Duration::from_secs(1),
        2,
    );
    let duplicate = StatusHost {
        name: "local".into(),
        target: None,
        workloads: vec![probe(second), probe(first), probe(second), probe(first)],
    };
    let run = service
        .try_start(StatusRequest {
            generation: 21,
            resources: false,
            hosts: vec![duplicate.clone(), duplicate],
        })
        .unwrap();
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let finished = matches!(message, StatusMessage::Finished { generation: 21 });
        messages.push(message);
        if finished {
            break;
        }
    }

    assert_eq!(fs::read_to_string(calls).unwrap(), "x");
    assert_eq!(
        messages
            .iter()
            .filter_map(|message| match message {
                StatusMessage::Workload { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![second, first],
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, StatusMessage::Host { .. }))
            .count(),
        1,
    );
}
