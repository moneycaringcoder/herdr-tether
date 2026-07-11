use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
    thread,
    time::Duration as StdDuration,
};

use chrono::{Duration, TimeZone, Utc};
use fs2::FileExt;
use herdr_tether::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    lifecycle::{
        CleanupEligibility, CloseOwnedError, ClosedWorkload, LifecycleService, cleanup_eligibility,
    },
    model::{ExternalSessionName, SessionId},
    quote::posix_quote,
    state::{SessionRecord, SessionStatus, State, StateStore},
    tmux::TmuxBackend,
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static FAKE_PROCESS_LOCK: Mutex<()> = Mutex::new(());

fn write_fake(path: &Path, log: &Path, stdout: &str, status: i32) {
    let script = format!(
        "#!/bin/sh\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nprintf '%s' '{stdout}'\nexit {status}\n",
        log = log.display(),
        stdout = stdout.replace('\'', "'\\''"),
    );
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(temporary, path).unwrap();
}

fn read_argv(log: &Path) -> Vec<String> {
    fs::read(log)
        .unwrap()
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8(value.to_vec()).unwrap())
        .collect()
}

fn wait_for_file(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(StdDuration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

fn assert_advisory_lock_available(path: &Path) {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.try_lock_exclusive()
        .expect("state advisory lock held during transport");
    FileExt::unlock(&file).unwrap();
}

fn id() -> SessionId {
    "tether-0197f198000070008000000000000001".parse().unwrap()
}

fn owned_record(status: SessionStatus) -> SessionRecord {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    SessionRecord {
        id: id(),
        host: "local".into(),
        target: "local".into(),
        directory: "/srv/code".into(),
        preset: Some("shell".into()),
        status,
        created_at: now,
        last_used_at: now,
        closed_at: (status == SessionStatus::Closed).then_some(now),
    }
}

fn lifecycle_fixture(
    tmux_body: &str,
) -> (
    tempfile::TempDir,
    StateStore,
    LifecycleService,
    std::path::PathBuf,
) {
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    write_fake(&tmux, &log, tmux_body, 0);
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );
    (temp, store, service, log)
}

#[test]
fn owned_close_inspects_without_state_lock_then_finalizes_missing() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let lock_path = temp.path().join(".state.json.lock");
    let started = temp.path().join("started");
    let release = temp.path().join("release");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    let script = format!(
        "#!/bin/sh\nprintf ok > '{started}'\nwhile [ ! -e '{release}' ]; do sleep 0.01; done\ncase \"$(cat '{state}')\" in *'\"status\": \"active\"'*) : ;; *) exit 70;; esac\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nexit 0\n",
        started = started.display(),
        release = release.display(),
        state = state_path.display(),
        log = log.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    let close = thread::spawn(move || service.close_owned(id()));
    wait_for_file(&started);
    assert_advisory_lock_available(&lock_path);
    fs::write(&release, []).unwrap();
    let result = close.join().unwrap().unwrap();

    assert_eq!(result.id, id());
    assert_eq!(result.workload, ClosedWorkload::Missing);
    let state = store.load().unwrap();
    let record = &state.sessions[0];
    assert_eq!(record.status, SessionStatus::Closed);
    assert_eq!(record.closed_at, Some(record.last_used_at));
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
fn owned_close_unknown_preserves_active_but_close_failure_leaves_closing() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    for (stdout, status, expected_unknown) in [
        ("malformed", 0, true),
        ("tether-0197f198000070008000000000000001\t0", 2, false),
    ] {
        let temp = tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let store = StateStore::new(state_path);
        store
            .save(&State {
                version: State::CURRENT_VERSION,

                sessions: vec![owned_record(SessionStatus::Active)],
            })
            .unwrap();
        let before = fs::read(temp.path().join("state.json")).unwrap();
        let tmux = temp.path().join("tmux");
        let log = temp.path().join("tmux.args");
        let script = format!(
            "#!/bin/sh\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\ncase \"$1\" in\nlist-sessions) printf '%s' '{stdout}'; exit 0;;\nkill-session) printf '\\033]0;spoof\\007close refused' >&2; exit {status};;\nesac\n",
            log = log.display(),
        );
        fs::write(&tmux, script).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let service = LifecycleService::new(
            store.clone(),
            ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
        );

        let error = service.close_owned(id()).unwrap_err();
        assert_eq!(
            matches!(error, CloseOwnedError::WorkloadUnknown(_)),
            expected_unknown
        );
        let after = fs::read(temp.path().join("state.json")).unwrap();
        let record = &store.load().unwrap().sessions[0];
        if expected_unknown {
            assert_eq!(
                after, before,
                "indeterminate inspect must not rewrite state"
            );
            assert_eq!(record.status, SessionStatus::Active);
        } else {
            assert_eq!(record.status, SessionStatus::Closing);
            let rendered = format!("{:#}", anyhow::Error::new(error));
            assert!(!rendered.contains('\u{1b}'));
            assert!(!rendered.contains("spoof"));
            assert!(rendered.contains("close refused"));
        }
        assert_eq!(record.closed_at, None);
    }
}

#[test]
fn owned_close_releases_state_lock_while_closing_exact_running_workload() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let lock_path = temp.path().join(".state.json.lock");
    let inspect_started = temp.path().join("inspect-started");
    let inspect_release = temp.path().join("inspect-release");
    let close_started = temp.path().join("close-started");
    let close_release = temp.path().join("close-release");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\nlist-sessions)\n  printf ok > '{inspect_started}'\n  while [ ! -e '{inspect_release}' ]; do sleep 0.01; done\n  case \"$(cat '{state}')\" in *'\"status\": \"active\"'*) : ;; *) exit 70;; esac\n  printf 'tether-0197f198000070008000000000000001\\t0';;\nkill-session)\n  printf ok > '{close_started}'\n  while [ ! -e '{close_release}' ]; do sleep 0.01; done\n  case \"$(cat '{state}')\" in *'\"status\": \"closing\"'*) : ;; *) exit 72;; esac;;\nesac\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nexit 0\n",
        inspect_started = inspect_started.display(),
        inspect_release = inspect_release.display(),
        close_started = close_started.display(),
        close_release = close_release.display(),
        state = state_path.display(),
        log = log.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    let close = thread::spawn(move || service.close_owned(id()));
    wait_for_file(&inspect_started);
    assert_advisory_lock_available(&lock_path);
    fs::write(&inspect_release, []).unwrap();
    wait_for_file(&close_started);
    assert_advisory_lock_available(&lock_path);
    fs::write(&close_release, []).unwrap();
    let result = close.join().unwrap().unwrap();

    assert_eq!(result.workload, ClosedWorkload::Terminated);
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Closed
    );
    let argv = read_argv(&log);
    assert!(argv.iter().any(|argument| argument == "list-sessions"));
    assert!(argv.ends_with(&[
        "kill-session".into(),
        "-t".into(),
        "=tether-0197f198000070008000000000000001".into(),
    ]));
}

#[test]
fn owned_close_retries_closing_and_rejects_unknown_or_closed_records() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let (_temp, store, service, _log) = lifecycle_fixture("");
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Closing;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        service.close_owned(id()).unwrap().workload,
        ClosedWorkload::Missing
    );
    assert!(matches!(
        service.close_owned(id()),
        Err(CloseOwnedError::AlreadyClosed(_))
    ));
    assert!(matches!(
        service.close_owned("tether-0197f198000070008000000000000002".parse().unwrap()),
        Err(CloseOwnedError::UnknownSession(_))
    ));
}

#[test]
fn owned_close_retry_from_closing_keeps_closing_on_unknown_inspect() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let (_temp, store, service, _log) = lifecycle_fixture("malformed");
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Closing;
            Ok(())
        })
        .unwrap();

    assert!(matches!(
        service.close_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Closing);
    assert_eq!(record.closed_at, None);
}

#[test]
fn owned_close_revalidates_exact_record_and_target_before_finalizing() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    for remove_record in [false, true] {
        let temp = tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let store = StateStore::new(state_path);
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![owned_record(SessionStatus::Active)],
            })
            .unwrap();
        let ready = temp.path().join("ready");
        let proceed = temp.path().join("proceed");
        let tmux = temp.path().join("tmux");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001\\t0';;\nkill-session) printf ready > '{ready}'; while test ! -e '{proceed}'; do sleep 0.01; done;;\nesac\nexit 0\n",
            ready = ready.display(),
            proceed = proceed.display(),
        );
        fs::write(&tmux, script).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let service = LifecycleService::new(
            store.clone(),
            ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
        );
        let worker = std::thread::spawn(move || service.close_owned(id()));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(ready.exists(), "close transport did not start");
        store
            .update(|state| {
                if remove_record {
                    state.sessions.clear();
                } else {
                    state.sessions[0].target = "changed.example".into();
                }
                Ok(())
            })
            .unwrap();
        fs::write(proceed, "").unwrap();

        assert!(matches!(
            worker.join().unwrap(),
            Err(CloseOwnedError::ConcurrentModification(_))
        ));
        let state = store.load().unwrap();
        if !remove_record {
            assert_eq!(state.sessions[0].status, SessionStatus::Closing);
            assert_eq!(state.sessions[0].closed_at, None);
            assert_eq!(state.sessions[0].target, "changed.example");
        }
    }
}

#[test]
fn owned_close_accepts_matching_record_already_finalized_by_peer() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let ready = temp.path().join("ready");
    let proceed = temp.path().join("proceed");
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001\\t0';;\nkill-session) printf ready > '{ready}'; while test ! -e '{proceed}'; do sleep 0.01; done;;\nesac\nexit 0\n",
        ready = ready.display(),
        proceed = proceed.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );
    let worker = std::thread::spawn(move || service.close_owned(id()));
    for _ in 0..500 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(ready.exists(), "close transport did not start");
    let peer_closed_at = Utc.with_ymd_and_hms(2026, 7, 10, 13, 0, 0).unwrap();
    store
        .update(|state| {
            let record = &mut state.sessions[0];
            record.status = SessionStatus::Closed;
            record.last_used_at = peer_closed_at;
            record.closed_at = Some(peer_closed_at);
            Ok(())
        })
        .unwrap();
    let peer_finalized = store.load().unwrap();
    fs::write(proceed, "").unwrap();

    assert_eq!(
        worker.join().unwrap().unwrap().workload,
        ClosedWorkload::Terminated
    );
    assert_eq!(
        store.load().unwrap(),
        peer_finalized,
        "idempotent finalization must not rewrite a peer's closed record"
    );
}

#[cfg(unix)]
fn assert_process_group_gone(pid_path: &Path) {
    let pid: libc::pid_t = fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..100 {
        // SAFETY: signal 0 only probes the process group recorded by the test child.
        let result = unsafe { libc::killpg(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("bounded transport left process group {pid} alive");
}

#[cfg(unix)]
#[test]
fn owned_close_times_out_hanging_inspect_without_mutating_active_record() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path.clone());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let before = fs::read(&state_path).unwrap();
    let tmux = temp.path().join("tmux");
    let process_group = temp.path().join("inspect.pgid");
    fs::write(
        &tmux,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nsleep 30 &\nwait\n",
            process_group.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    let started = std::time::Instant::now();
    assert!(matches!(
        service.close_owned(id()),
        Err(CloseOwnedError::Inspect { .. })
    ));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "inspect transport exceeded its fixed deadline"
    );
    assert_eq!(fs::read(&state_path).unwrap(), before);
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Active
    );
    assert_process_group_gone(&process_group);
}

#[cfg(unix)]
#[test]
fn owned_close_times_out_hanging_exact_close_and_leaves_closing() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Active)],
        })
        .unwrap();
    let tmux = temp.path().join("tmux");
    let process_group = temp.path().join("close.pgid");
    fs::write(
        &tmux,
        format!(
            "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001\\t0';;\nkill-session) printf '%s' \"$$\" > '{}'; sleep 30 & wait;;\nesac\n",
            process_group.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    let started = std::time::Instant::now();
    assert!(matches!(
        service.close_owned(id()),
        Err(CloseOwnedError::Close { .. })
    ));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "close transport exceeded its fixed deadline"
    );
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Closing);
    assert_eq!(record.closed_at, None);
    assert_process_group_gone(&process_group);
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
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
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
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
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
fn external_attach_is_exact_and_has_no_lifecycle_operation() {
    let name = "-work '$(touch nope)'`echo`"
        .parse::<ExternalSessionName>()
        .unwrap();
    let binaries = ProcessBinaries::new("ssh", "tmux");
    let local = TmuxBackend::local(binaries.clone())
        .attach_external_command(&name)
        .unwrap();
    assert_eq!(
        local.args,
        ["attach-session", "-t", "=-work '$(touch nope)'`echo`"]
    );

    let remote = TmuxBackend::remote("build-box", binaries)
        .unwrap()
        .attach_external_command(&name)
        .unwrap();
    let expected_target = posix_quote(&format!("={name}")).unwrap();
    assert_eq!(
        remote.args.last().unwrap(),
        &format!("'tmux' 'attach-session' '-t' {expected_target}")
    );
    assert!(remote.args.iter().any(|argument| argument == "-t"));
    assert!(!remote.args.iter().any(|argument| {
        argument.contains("kill-session")
            || argument.contains("new-session")
            || argument.contains("rename")
    }));
}

#[test]
fn unsafe_or_reserved_external_names_are_rejected() {
    for name in [
        "",
        "tether-manual",
        "bad\tname",
        "bad\nname",
        "bad:name",
        "bad.name",
        "$0",
        "bidi\u{202e}name",
        "café",
    ] {
        assert!(name.parse::<ExternalSessionName>().is_err(), "{name:?}");
    }
    assert!(
        "x".repeat(ExternalSessionName::MAX_BYTES + 1)
            .parse::<ExternalSessionName>()
            .is_err()
    );
}

#[test]
fn local_backend_uses_argv_boundaries_and_exact_tmux_targets() {
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
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
    let _guard = FAKE_PROCESS_LOCK.lock().unwrap();
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
