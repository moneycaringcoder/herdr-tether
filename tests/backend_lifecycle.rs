use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use chrono::{Duration, TimeZone, Utc};
use herdr_tether::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    lifecycle::{CleanupEligibility, cleanup_eligibility},
    model::SessionId,
    quote::posix_quote,
    state::{SessionRecord, SessionStatus},
    tmux::TmuxBackend,
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_fake(path: &Path, log: &Path, stdout: &str, status: i32) {
    let script = format!(
        "#!/bin/sh\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nprintf '%s' '{stdout}'\nexit {status}\n",
        log = log.display(),
        stdout = stdout.replace('\'', "'\\''"),
    );
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn read_argv(log: &Path) -> Vec<String> {
    fs::read(log)
        .unwrap()
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8(value.to_vec()).unwrap())
        .collect()
}

fn id() -> SessionId {
    "tether-0197f198000070008000000000000001".parse().unwrap()
}

#[test]
fn posix_quote_handles_adversarial_values_without_interpolation() {
    let cases = [
        ("", "''"),
        ("plain", "'plain'"),
        ("it's", "'it'\\''s'"),
        (
            "$(touch /tmp/nope); `id` | & < > *",
            "'$(touch /tmp/nope); `id` | & < > *'",
        ),
        ("line one\nline two\t$HOME", "'line one\nline two\t$HOME'"),
        ("héllø", "'héllø'"),
    ];

    for (input, expected) in cases {
        assert_eq!(posix_quote(input).unwrap(), expected);
    }
    assert!(posix_quote("nul\0byte").is_err());
}

#[test]
fn remote_create_passes_one_fully_quoted_command_to_fake_ssh() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("ssh.args");
    write_fake(&ssh, &log, "", 0);
    write_fake(&tmux, &temp.path().join("tmux.args"), "", 0);
    let backend = TmuxBackend::remote(
        "ssh://builder@example.test:2222",
        ProcessBinaries::new(ssh, tmux),
    )
    .unwrap();
    let directory = "/srv/it's $(touch /tmp/nope)\nline";
    let command = "printf '%s\\n' \"$HOME\"; echo `id`";

    backend
        .create(&LaunchSpec {
            id: id(),
            directory: directory.into(),
            command: command.into(),
        })
        .unwrap();

    let argv = read_argv(&log);
    assert_eq!(
        &argv[..9],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            "ssh://builder@example.test:2222",
            "'tmux' 'new-session' '-d' '-s' 'tether-0197f198000070008000000000000001' '-c' '/srv/it'\\''s $(touch /tmp/nope)\nline' '--' '/bin/sh' '-lc' 'printf '\\''%s\\n'\\'' \"$HOME\"; echo `id`'",
        ]
    );
    assert_eq!(
        argv.len(),
        9,
        "ssh must receive one remote command argument"
    );
}

#[test]
fn attach_only_attaches_and_close_is_the_only_kill_path() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("ssh.args");
    write_fake(&ssh, &log, "", 0);
    write_fake(&tmux, &temp.path().join("tmux.args"), "", 0);
    let backend = TmuxBackend::remote("build-box", ProcessBinaries::new(ssh, tmux)).unwrap();

    let attach = backend.attach_command(&id()).unwrap();
    assert_eq!(attach.program.file_name().unwrap(), "ssh");
    assert_eq!(
        attach.args.last().unwrap(),
        "'tmux' 'attach-session' '-t' '=tether-0197f198000070008000000000000001'"
    );
    assert!(!attach.args.iter().any(|arg| arg.contains("kill")));

    backend.close(&id()).unwrap();
    let close_argv = read_argv(&log);
    assert_eq!(
        close_argv.last().unwrap(),
        "'tmux' 'kill-session' '-t' '=tether-0197f198000070008000000000000001'"
    );
}

#[test]
fn local_backend_uses_argv_boundaries_and_exact_tmux_targets() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    write_fake(&ssh, &temp.path().join("ssh.args"), "", 0);
    write_fake(&tmux, &log, "tether-0197f198000070008000000000000001\t2", 0);
    let backend = TmuxBackend::local(ProcessBinaries::new(ssh, tmux));

    assert_eq!(
        backend.inspect(&id()).unwrap(),
        WorkloadState::Running { attached: 2 }
    );
    assert_eq!(
        read_argv(&log),
        [
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}",
            "-f",
            "#{==:#{session_name},tether-0197f198000070008000000000000001}",
        ]
    );
}

#[test]
fn inspect_maps_missing_and_failure_results() {
    for (status, expected) in [
        (0, WorkloadState::Missing),
        (1, WorkloadState::Missing),
        (2, WorkloadState::Unknown),
        (255, WorkloadState::Unknown),
    ] {
        let temp = tempdir().unwrap();
        let tmux = temp.path().join("tmux");
        write_fake(&tmux, &temp.path().join("tmux.args"), "", status);
        let backend =
            TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

        assert_eq!(
            backend.inspect(&id()).unwrap(),
            expected,
            "unexpected inspect state for exit status {status}"
        );
    }
}

#[test]
fn explicit_ssh_targets_reject_option_and_shell_injection() {
    let binaries = ProcessBinaries::new("ssh", "tmux");
    for target in [
        "",
        "-oProxyCommand=bad",
        "host name",
        "host;touch-nope",
        "host\nname",
    ] {
        assert!(
            TmuxBackend::remote(target, binaries.clone()).is_err(),
            "accepted {target:?}"
        );
    }
    assert!(TmuxBackend::remote("user@example.test", binaries.clone()).is_ok());
    assert!(TmuxBackend::remote("ssh://user@example.test:2222", binaries.clone()).is_ok());
    for target in ["SSH://user@example.test", "http://user@example.test"] {
        assert!(
            TmuxBackend::remote(target, binaries.clone()).is_err(),
            "accepted mismatched URI scheme {target:?}"
        );
    }
}

#[test]
fn cleanup_never_selects_active_unknown_or_recent_sessions() {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let mut record = SessionRecord {
        id: id(),
        host: "build-box".into(),
        target: "builder@example.test".into(),
        directory: "/srv/code".into(),
        preset: Some("shell".into()),
        status: SessionStatus::Active,
        created_at: now - Duration::days(30),
        last_used_at: now - Duration::days(30),
        closed_at: None,
    };

    assert_eq!(
        cleanup_eligibility(
            &record,
            WorkloadState::Running { attached: 0 },
            now,
            Duration::days(7)
        ),
        CleanupEligibility::KeepActive
    );
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Unknown, now, Duration::days(7)),
        CleanupEligibility::KeepUnknown
    );
    record.status = SessionStatus::Closed;
    record.closed_at = Some(now - Duration::days(1));
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Missing, now, Duration::days(7)),
        CleanupEligibility::KeepRecent
    );
    record.closed_at = Some(now - Duration::days(8));
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Missing, now, Duration::days(7)),
        CleanupEligibility::RemoveMetadata
    );
    assert_eq!(
        cleanup_eligibility(
            &record,
            WorkloadState::Running { attached: 0 },
            now,
            Duration::days(7)
        ),
        CleanupEligibility::KeepActive,
        "prune must not kill a workload that still exists"
    );
}

#[test]
fn command_spec_quotes_program_and_arguments_for_herdr_pane_run() {
    let command = CommandSpec {
        program: "/tmp/plugin root/herdr-tether".into(),
        args: vec!["session".into(), "resume".into(), id().to_string()],
    };
    assert_eq!(
        command.posix_command_line().unwrap(),
        "'/tmp/plugin root/herdr-tether' 'session' 'resume' 'tether-0197f198000070008000000000000001'"
    );
}
