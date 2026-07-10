use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use herdr_tether::{
    backend::ProcessBinaries,
    model::SessionId,
    status::{
        HostReachability, StatusHost, StatusMessage, StatusRequest, StatusService, WorkloadStatus,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_fake_ssh(path: &Path, fast_id: SessionId) {
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" fast "*) printf '%s\t2\n' '{fast_id}' ;;
  *" slow "*) sleep 2 ;;
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
    let run = service.start(StatusRequest {
        generation: 7,
        hosts: vec![
            StatusHost {
                name: "slow".into(),
                target: Some("slow".into()),
                workloads: vec![slow_id],
            },
            StatusHost {
                name: "fast".into(),
                target: Some("fast".into()),
                workloads: vec![fast_id],
            },
        ],
    });

    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
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
    assert!(started.elapsed() < Duration::from_millis(1500));
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
    let run = service.start(StatusRequest {
        generation: 1,
        hosts: vec![StatusHost {
            name: "slow".into(),
            target: Some("slow".into()),
            workloads: vec![workload],
        }],
    });

    run.cancel();
    let started = Instant::now();
    while let Ok(message) = run.receiver.recv_timeout(Duration::from_millis(100)) {
        assert!(matches!(message, StatusMessage::Finished { generation: 1 }));
    }
    assert!(started.elapsed() < Duration::from_secs(1));
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
  malformed) printf 'tether-not-an-id\t0\n' ;;
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
        Duration::from_secs(1),
        3,
    );
    let run = service.start(StatusRequest {
        generation: 9,
        hosts: vec![
            StatusHost {
                name: "offline".into(),
                target: Some("offline".into()),
                workloads: vec![offline_id],
            },
            StatusHost {
                name: "empty".into(),
                target: Some("empty".into()),
                workloads: vec![empty_id],
            },
            StatusHost {
                name: "malformed".into(),
                target: Some("malformed".into()),
                workloads: vec![malformed_id],
            },
        ],
    });
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
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
            status: WorkloadStatus::Unknown,
            ..
        } if *id == malformed_id
    )));
}
