use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    thread,
    time::Duration as StdDuration,
};

use chrono::{Duration, TimeZone, Utc};
use fs2::FileExt;
use herdr_tether::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    lifecycle::{
        CleanupEligibility, CloseOwnedError, ClosedWorkload, LifecycleService, PruneError,
        PruneService, cleanup_eligibility,
    },
    model::{ExternalSessionName, OwnershipProof, SessionId},
    quote::posix_quote,
    state::{SessionRecord, SessionStatus, State, StateStore},
    tmux::TmuxBackend,
};
use parking_lot::Mutex;
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
    for _ in 0..1_000 {
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

fn proof() -> OwnershipProof {
    "0197f198000070008000000000000002".parse().unwrap()
}

fn owned_record(status: SessionStatus) -> SessionRecord {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    SessionRecord {
        id: id(),
        host: "local".into(),
        target: "local".into(),
        directory: "/srv/code".into(),
        preset: Some("shell".into()),
        command: Some("exec shell".into()),
        tmux_session_id: None,
        ownership_proof: Some(proof()),
        status,
        created_at: now,
        last_used_at: now,
        closed_at: (status == SessionStatus::Ended).then_some(now),
        exit_status: None,
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
            sessions: vec![owned_record(SessionStatus::Running)],
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
fn owned_record_reread_is_authoritative_and_transport_free() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let record = owned_record(SessionStatus::Stopping);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![record.clone()],
        })
        .unwrap();
    let service = LifecycleService::new(
        store,
        ProcessBinaries::new(
            temp.path().join("must-not-run-ssh"),
            temp.path().join("must-not-run-tmux"),
        ),
    );

    assert_eq!(service.owned_record(id()).unwrap(), Some(record));
    assert_eq!(
        service
            .owned_record("tether-0197f198000070008000000000000009".parse().unwrap())
            .unwrap(),
        None
    );
}

#[test]
fn owned_close_inspects_without_state_lock_then_finalizes_missing() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let lock_path = temp.path().join(".state.json.lock");
    let started = temp.path().join("started");
    let release = temp.path().join("release");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    let script = format!(
        "#!/bin/sh\nprintf ok > '{started}'\nwhile [ ! -e '{release}' ]; do sleep 0.01; done\ncase \"$(cat '{state}')\" in *'\"status\": \"running\"'*) : ;; *) exit 70;; esac\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nexit 0\n",
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
            sessions: vec![owned_record(SessionStatus::Running)],
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
    assert_eq!(record.status, SessionStatus::Ended);
    assert_eq!(record.closed_at, Some(record.last_used_at));
    assert_eq!(
        read_argv(&log),
        [
            "list-sessions",
            "-F",
            "#{session_name}:#{session_id}:#{session_attached}:#{pane_dead}:#{pane_dead_status}:#{TETHER_OWNERSHIP_PROOF}",
            "-f",
            "#{&&:#{==:#{session_name},tether-0197f198000070008000000000000001},#{==:#{TETHER_OWNERSHIP_PROOF},0197f198000070008000000000000002}}",
        ]
    );
}

#[test]
fn owned_close_unknown_preserves_active_but_close_failure_leaves_closing() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    for (stdout, status, expected_unknown) in [
        ("malformed", 0, true),
        (
            "tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002",
            2,
            false,
        ),
    ] {
        let temp = tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let store = StateStore::new(state_path);
        store
            .save(&State {
                version: State::CURRENT_VERSION,

                sessions: vec![owned_record(SessionStatus::Running)],
            })
            .unwrap();
        let before = fs::read(temp.path().join("state.json")).unwrap();
        let tmux = temp.path().join("tmux");
        let log = temp.path().join("tmux.args");
        let script = format!(
            "#!/bin/sh\n: > '{log}'\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\ncase \"$1\" in\nlist-sessions) printf '%s' '{stdout}'; exit 0;;\nif-shell) printf '\\033]0;spoof\\007close refused TETHER_OWNERSHIP_PROOF=0197f198000070008000000000000002' >&2; exit {status};;\nesac\n",
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
            assert_eq!(record.status, SessionStatus::Running);
        } else {
            assert_eq!(record.status, SessionStatus::Stopping);
            let rendered = format!("{:#}", anyhow::Error::new(error));
            assert!(!rendered.contains('\u{1b}'));
            assert!(!rendered.contains("spoof"));
            assert!(rendered.contains("close refused"));
            assert!(!rendered.contains("0197f198000070008000000000000002"));
            assert!(!rendered.contains("TETHER_OWNERSHIP_PROOF"));
        }
        assert_eq!(record.closed_at, None);
    }
}

#[test]
fn owned_close_releases_state_lock_while_closing_exact_running_workload() {
    let _guard = FAKE_PROCESS_LOCK.lock();
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
        "#!/bin/sh\ncase \"$1\" in\nlist-sessions)\n  if [ ! -e '{inspect_started}' ]; then\n    case \"$(cat '{state}')\" in *'\"status\": \"running\"'*) : ;; *) exit 70;; esac\n    printf ok > '{inspect_started}'\n    while [ ! -e '{inspect_release}' ]; do sleep 0.01; done\n  fi\n  printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002';;\nif-shell)\n  printf ok > '{close_started}'\n  while [ ! -e '{close_release}' ]; do sleep 0.01; done\n  case \"$(cat '{state}')\" in *'\"status\": \"stopping\"'*) : ;; *) exit 72;; esac;;\nesac\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\nexit 0\n",
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
            sessions: vec![owned_record(SessionStatus::Running)],
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
        SessionStatus::Ended
    );
    let argv = read_argv(&log);
    assert!(argv.iter().any(|argument| argument == "list-sessions"));
    assert!(argv.iter().any(|argument| argument == "if-shell"));
    assert!(argv.iter().any(|argument| argument == "$7"));
    assert!(argv.iter().any(|argument| argument == "kill-session -t $7"));
}

#[test]
fn owned_close_retries_closing_and_rejects_unknown_or_closed_records() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (_temp, store, service, _log) = lifecycle_fixture("");
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Stopping;
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
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (_temp, store, service, _log) = lifecycle_fixture("malformed");
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Stopping;
            Ok(())
        })
        .unwrap();

    assert!(matches!(
        service.close_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Stopping);
    assert_eq!(record.closed_at, None);
}

#[test]
fn owned_close_revalidates_exact_record_and_target_before_finalizing() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    for remove_record in [false, true] {
        let temp = tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let store = StateStore::new(state_path);
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![owned_record(SessionStatus::Running)],
            })
            .unwrap();
        let ready = temp.path().join("ready");
        let proceed = temp.path().join("proceed");
        let tmux = temp.path().join("tmux");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002';;\nif-shell) printf ready > '{ready}'; while test ! -e '{proceed}'; do sleep 0.01; done;;\nesac\nexit 0\n",
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
        wait_for_file(&ready);
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
            assert_eq!(state.sessions[0].status, SessionStatus::Stopping);
            assert_eq!(state.sessions[0].closed_at, None);
            assert_eq!(state.sessions[0].target, "changed.example");
        }
    }
}

#[test]
fn owned_close_accepts_matching_record_already_finalized_by_peer() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
        })
        .unwrap();
    let ready = temp.path().join("ready");
    let proceed = temp.path().join("proceed");
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002';;\nif-shell) printf ready > '{ready}'; while test ! -e '{proceed}'; do sleep 0.01; done;;\nesac\nexit 0\n",
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
    wait_for_file(&ready);
    let peer_closed_at = Utc.with_ymd_and_hms(2026, 7, 10, 13, 0, 0).unwrap();
    store
        .update(|state| {
            let record = &mut state.sessions[0];
            record.status = SessionStatus::Ended;
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
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path.clone());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
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
        SessionStatus::Running
    );
    assert_process_group_gone(&process_group);
}

#[cfg(unix)]
#[test]
fn owned_close_times_out_hanging_exact_close_and_leaves_closing() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
        })
        .unwrap();
    let tmux = temp.path().join("tmux");
    let process_group = temp.path().join("close.pgid");
    fs::write(
        &tmux,
        format!(
            "#!/bin/sh\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002';;\nif-shell) printf '%s' \"$$\" > '{}'; sleep 30 & wait;;\nesac\n",
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
    assert_eq!(record.status, SessionStatus::Stopping);
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
fn process_binaries_resolve_tools_from_a_restricted_absolute_path() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    write_fake(&ssh, &temp.path().join("ssh.args"), "", 0);
    write_fake(&tmux, &temp.path().join("tmux.args"), "", 0);
    let previous_path = std::env::var_os("PATH");
    // SAFETY: process environment mutation is serialized by FAKE_PROCESS_LOCK.
    unsafe { std::env::set_var("PATH", temp.path()) };
    let binaries = ProcessBinaries::new("ssh", "tmux");
    match previous_path {
        Some(path) => {
            // SAFETY: process environment mutation is serialized by FAKE_PROCESS_LOCK.
            unsafe { std::env::set_var("PATH", path) };
        }
        None => {
            // SAFETY: process environment mutation is serialized by FAKE_PROCESS_LOCK.
            unsafe { std::env::remove_var("PATH") };
        }
    }

    assert_eq!(binaries.ssh(), ssh);
    assert_eq!(binaries.tmux(), tmux);
}

#[test]
fn missing_local_tmux_reports_install_and_search_guidance() {
    let backend = TmuxBackend::local(ProcessBinaries::new(
        "unused-ssh-for-missing-tool-test",
        "tmux-that-does-not-exist-for-test",
    ));
    let error = backend.inspect(&id(), &proof()).unwrap_err().to_string();
    assert!(error.contains("tmux-that-does-not-exist-for-test"));
    assert!(error.contains("install the tool or make it executable"));
    assert!(error.contains("/opt/homebrew/bin"));
}

#[test]
fn owned_create_verifies_exact_cwd_sets_session_mouse_and_rolls_back_mismatch() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    for (reported_cwd, succeeds) in [("/work/repo", true), ("/work/other", false)] {
        let temp = tempdir().unwrap();
        let tmux = temp.path().join("tmux");
        let log = temp.path().join("calls");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nif [ \"$1\" = new-session ]; then printf '$7:%%3'; elif [ \"$1\" = display-message ]; then printf '%s' '{reported_cwd}'; fi\n",
            log = log.display(),
        );
        fs::write(&tmux, script).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let backend =
            TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));
        let result = backend.create(&LaunchSpec {
            id: id(),
            ownership_proof: proof(),
            directory: "/work/repo".into(),
            command: "printf ready".into(),
        });
        assert_eq!(result.is_ok(), succeeds);

        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains(
            "new-session -d -s tether-0197f198000070008000000000000001 -c /work/repo -e TETHER_OWNERSHIP_PROOF=0197f198000070008000000000000002 -P -F #{session_id}:#{pane_id} -- /bin/sh -lc"
        ));
        assert!(
            calls.contains(
                "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" && exec /bin/sh -c \"$2\""
            ),
            "{calls:?}"
        );
        assert!(calls.contains("set-option -p -t %3 remain-on-exit on"));
        assert!(calls.contains("display-message -p -t %3 #{pane_current_path}"));
        if succeeds {
            assert!(calls.contains("set-option -t $7 mouse on"));
            assert!(!calls.contains("kill-session"));
        } else {
            assert!(result.unwrap_err().to_string().contains("cwd mismatch"));
            assert!(calls.contains("kill-session -t $7"));
            assert!(!calls.contains("set-option -t $7 mouse on"));
        }
    }
}

#[cfg(unix)]
#[test]
fn owned_create_accepts_symlinked_directory_with_same_inode() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let real = temp.path().join("real-repo");
    let selected = temp.path().join("selected-repo");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &selected).unwrap();
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("calls");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nif [ \"$1\" = new-session ]; then printf '$7:%%3'; elif [ \"$1\" = display-message ]; then printf '%s' '{real}'; fi\n",
        log = log.display(),
        real = real.display(),
    );
    fs::write(&tmux, script).unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    backend
        .create(&LaunchSpec {
            id: id(),
            ownership_proof: proof(),
            directory: selected.to_string_lossy().into_owned(),
            command: "sleep 10".into(),
        })
        .unwrap();

    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("set-option -t $7 mouse on")
    );
}

#[test]
fn owned_resume_enables_mouse_but_external_attach_never_mutates_options() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    let target = format!("{}:$7", id());
    write_fake(&tmux, &log, &target, 0);
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let owned = backend
        .attach_command(&id(), &proof(), "$7".parse().unwrap())
        .unwrap();
    let attach_argv = owned.args;
    assert_eq!(attach_argv[0], "if-shell");
    assert_eq!(attach_argv[2], "$7");
    assert!(attach_argv[4].contains("#{session_id},$7"));
    assert!(attach_argv[4].contains(&id().to_string()));
    assert!(attach_argv[4].contains(&proof().to_string()));
    assert_eq!(
        attach_argv[5],
        "set-option -t $7 mouse on ; attach-session -t $7"
    );
    assert!(attach_argv[6].contains("TETHER_OWNERSHIP_GUARD_REJECTED"));
    assert!(attach_argv[6].contains("exit 75"));

    let external = "work".parse::<ExternalSessionName>().unwrap();
    let command = backend.attach_external_command(&external).unwrap();
    assert_eq!(command.args, ["attach-session", "-t", "=work"]);
    assert!(!log.exists(), "external attach must not execute set-option");
}

#[test]
fn remote_create_passes_one_fully_quoted_command_to_fake_ssh() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("ssh.args");
    let calls = temp.path().join("ssh.calls");
    let directory = "/srv/it's $(touch /tmp/nope)\nline";
    let command = "printf '%s\\n' \"$HOME\"; echo `id`";
    let escaped_directory = "/canonical/repo";
    let script = format!(
        "#!/bin/sh\nfor remote do :; done\nprintf '%s\\n' \"$remote\" >> '{calls}'\ncase \"$remote\" in\n  *\"'new-session'\"*) : > '{log}'; for arg do printf '%s\\000' \"$arg\" >> '{log}'; done; printf '$7:%%3' ;;\n  *\"'#{{pane_current_path}}'\"*) printf '%s' '{escaped_directory}' ;;\nesac\n",
        log = log.display(),
        calls = calls.display(),
    );
    fs::write(&ssh, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    write_fake(&tmux, &temp.path().join("tmux.args"), "", 0);
    let backend = TmuxBackend::remote(
        "ssh://builder@example.test:2222",
        ProcessBinaries::new(ssh, tmux),
    )
    .unwrap();

    backend
        .create(&LaunchSpec {
            id: id(),
            ownership_proof: proof(),
            directory: directory.into(),
            command: command.into(),
        })
        .unwrap();

    let argv = read_argv(&log);
    assert_eq!(
        &argv[..10],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-p",
            "2222",
            "--",
            "builder@example.test",
        ]
    );
    assert_eq!(
        argv.len(),
        11,
        "ssh must receive one remote command argument"
    );
    let launch_script = "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" && exec /bin/sh -c \"$2\"";
    let remote = &argv[10];
    for expected in [
        "'tmux' 'new-session' '-d'",
        &posix_quote(directory).unwrap(),
        "'TETHER_OWNERSHIP_PROOF=0197f198000070008000000000000002'",
        &posix_quote(launch_script).unwrap(),
        &posix_quote(command).unwrap(),
    ] {
        assert!(
            remote.contains(expected),
            "missing {expected:?} in {remote:?}"
        );
    }
    let calls = fs::read_to_string(calls).unwrap();
    let compare_script = "actual=$1; expected=$2; case \"$actual\" in '~') actual=$HOME ;; '~/'*) actual=$HOME${actual#\\~} ;; esac; case \"$expected\" in '~') expected=$HOME ;; '~/'*) expected=$HOME${expected#\\~} ;; esac; [ \"$actual\" -ef \"$expected\" ]";
    assert!(calls.contains(&posix_quote(launch_script).unwrap()));
    assert!(calls.contains(&posix_quote(compare_script).unwrap()));
    assert!(calls.contains("'tmux' 'display-message' '-p' '-t' '%3' '#{pane_current_path}'"));
    assert!(calls.contains("'tmux' 'set-option' '-p' '-t' '%3' 'remain-on-exit' 'on'"));
    assert!(calls.contains("'tmux' 'set-option' '-t' '$7' 'mouse' 'on'"));
}

#[test]
fn remote_ipv6_uri_maps_authority_and_port_to_openssh_argv() {
    let backend = TmuxBackend::remote(
        "ssh://builder@[2001:db8::10]:2222",
        ProcessBinaries::new("/usr/bin/ssh", "/usr/bin/tmux"),
    )
    .unwrap();

    let command = backend
        .attach_external_command(&"work".parse().unwrap())
        .unwrap();
    assert_eq!(
        &command.args[..11],
        [
            "-o",
            "BatchMode=yes",
            "-t",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-p",
            "2222",
            "--",
            "builder@2001:db8::10",
        ]
    );
    assert_eq!(command.args[11], "'tmux' 'attach-session' '-t' '=work'");
}

#[test]
fn remote_create_error_redacts_ownership_proof_and_terminal_controls() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let script = "#!/bin/sh\nremote=$9\nprintf '\\033]2;spoofed title\\007\\033[31m%s\\033[0m\\n' \"$remote\" >&2\nexit 23\n";
    fs::write(&ssh, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::remote(
        "builder@example.test",
        ProcessBinaries::new(ssh, temp.path().join("unused-tmux")),
    )
    .unwrap();

    let error = format!(
        "{:#}",
        backend
            .create(&LaunchSpec {
                id: id(),
                ownership_proof: proof(),
                directory: "/srv/private".into(),
                command: "sleep 10".into(),
            })
            .unwrap_err()
    );

    assert!(error.contains("tmux create failed"));
    assert!(!error.contains(&proof().to_string()), "{error}");
    assert!(!error.contains("TETHER_OWNERSHIP_PROOF"), "{error}");
    assert!(!error.contains('\u{1b}'), "{error:?}");
    assert!(!error.contains("spoofed title"), "{error}");
}

#[test]
fn attach_only_attaches_and_close_is_the_only_kill_path() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("ssh.args");
    let target = format!("{}:$7", id());
    write_fake(&ssh, &log, &target, 0);
    write_fake(&tmux, &temp.path().join("tmux.args"), "", 0);
    let backend = TmuxBackend::remote("build-box", ProcessBinaries::new(ssh, tmux)).unwrap();

    let attach = backend
        .attach_command(&id(), &proof(), "$7".parse().unwrap())
        .unwrap();
    assert_eq!(attach.program.file_name().unwrap(), "ssh");
    let remote_attach = attach.args.last().unwrap();
    assert!(remote_attach.contains("'tmux' 'if-shell' '-t' '$7' '-F'"));
    assert!(remote_attach.contains("'set-option -t $7 mouse on ; attach-session -t $7'"));
    assert!(!remote_attach.contains("'kill-session"));

    backend
        .close(&id(), &proof(), "$7".parse().unwrap())
        .unwrap();
    let close_argv = read_argv(&log);
    let remote_close = close_argv.last().unwrap();
    assert!(remote_close.contains("'tmux' 'if-shell' '-t' '$7' '-F'"));
    assert!(remote_close.contains("'kill-session -t $7'"));
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
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    write_fake(&ssh, &temp.path().join("ssh.args"), "", 0);
    write_fake(
        &tmux,
        &log,
        "tether-0197f198000070008000000000000001:$7:2:0::0197f198000070008000000000000002",
        0,
    );
    let backend = TmuxBackend::local(ProcessBinaries::new(ssh, tmux));

    assert_eq!(
        backend.inspect(&id(), &proof()).unwrap(),
        WorkloadState::Running {
            attached: 2,
            identity: "$7".parse().unwrap()
        }
    );
    assert_eq!(
        read_argv(&log),
        [
            "list-sessions",
            "-F",
            "#{session_name}:#{session_id}:#{session_attached}:#{pane_dead}:#{pane_dead_status}:#{TETHER_OWNERSHIP_PROOF}",
            "-f",
            "#{&&:#{==:#{session_name},tether-0197f198000070008000000000000001},#{==:#{TETHER_OWNERSHIP_PROOF},0197f198000070008000000000000002}}",
        ]
    );
}

#[test]
fn inspect_maps_missing_and_failure_results() {
    let _guard = FAKE_PROCESS_LOCK.lock();
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
            backend.inspect(&id(), &proof()).unwrap(),
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
        command: Some("exec shell".into()),
        tmux_session_id: None,
        ownership_proof: Some(proof()),
        status: SessionStatus::Running,
        created_at: now - Duration::days(30),
        last_used_at: now - Duration::days(30),
        closed_at: None,
        exit_status: None,
    };

    assert_eq!(
        cleanup_eligibility(
            &record,
            WorkloadState::Running {
                attached: 0,
                identity: "$7".parse().unwrap()
            },
            now,
            Duration::days(7)
        ),
        CleanupEligibility::KeepActive
    );
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Unknown, now, Duration::days(7)),
        CleanupEligibility::KeepUnknown
    );
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Missing, now, Duration::days(7)),
        CleanupEligibility::KeepActive
    );
    record.status = SessionStatus::Stopping;
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Missing, now, Duration::days(7)),
        CleanupEligibility::KeepActive
    );
    record.status = SessionStatus::Ended;
    assert_eq!(
        cleanup_eligibility(&record, WorkloadState::Missing, now, Duration::days(7)),
        CleanupEligibility::KeepRecent,
        "closed metadata without closed_at is retained conservatively"
    );
    record.status = SessionStatus::Ended;
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
            WorkloadState::Running {
                attached: 0,
                identity: "$7".parse().unwrap()
            },
            now,
            Duration::days(7)
        ),
        CleanupEligibility::KeepActive,
        "prune must not kill a workload that still exists"
    );
}

fn prune_record(
    id: &str,
    status: SessionStatus,
    closed_at: Option<chrono::DateTime<Utc>>,
) -> SessionRecord {
    let created_at = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    SessionRecord {
        id: id.parse().unwrap(),
        host: "local".into(),
        target: "local".into(),
        directory: "/srv/code".into(),
        preset: None,
        command: Some("exec shell".into()),
        tmux_session_id: None,
        ownership_proof: Some(proof()),
        status,
        created_at,
        last_used_at: closed_at.unwrap_or(created_at),
        closed_at,
        exit_status: None,
    }
}

#[test]
fn prune_preview_selects_exact_cutoff_and_excludes_ineligible_records() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let cutoff = now - Duration::days(7);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                prune_record(
                    "tether-0197f198000070008000000000000011",
                    SessionStatus::Ended,
                    Some(cutoff),
                ),
                prune_record(
                    "tether-0197f198000070008000000000000012",
                    SessionStatus::Ended,
                    Some(cutoff + Duration::seconds(1)),
                ),
                prune_record(
                    "tether-0197f198000070008000000000000013",
                    SessionStatus::Running,
                    None,
                ),
                prune_record(
                    "tether-0197f198000070008000000000000014",
                    SessionStatus::Stopping,
                    None,
                ),
            ],
        })
        .unwrap();

    let preview = PruneService::new(store).preview_at(7, now).unwrap();
    assert_eq!(preview.older_than_days(), 7);
    assert_eq!(preview.captured_at(), now);
    assert_eq!(
        preview.ids(),
        &["tether-0197f198000070008000000000000011".parse().unwrap()]
    );
}

#[test]
fn prune_preview_accepts_explicit_zero_and_rejects_duration_overflow() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![prune_record(
                "tether-0197f198000070008000000000000011",
                SessionStatus::Ended,
                Some(now),
            )],
        })
        .unwrap();
    let service = PruneService::new(store);

    assert_eq!(service.preview_at(0, now).unwrap().ids().len(), 1);
    assert!(matches!(
        service.preview_at(u64::MAX, now),
        Err(PruneError::RetentionTooLarge(u64::MAX))
    ));
}

#[test]
fn confirmed_prune_uses_stable_preview_and_never_removes_newly_eligible_records() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let first = "tether-0197f198000070008000000000000011";
    let later = "tether-0197f198000070008000000000000012";
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                prune_record(first, SessionStatus::Ended, Some(now - Duration::days(8))),
                prune_record(later, SessionStatus::Ended, Some(now - Duration::days(6))),
            ],
        })
        .unwrap();
    let service = PruneService::new(store.clone());
    let preview = service.preview_at(7, now).unwrap();
    store
        .update(|state| {
            state.sessions[1].closed_at = Some(now - Duration::days(9));
            state.sessions[1].last_used_at = now - Duration::days(9);
            Ok(())
        })
        .unwrap();

    let result = service.apply(&preview).unwrap();
    assert_eq!(result.removed_ids, vec![first.parse().unwrap()]);
    assert!(result.skipped_ids.is_empty());
    assert_eq!(
        store.load().unwrap().sessions,
        vec![prune_record(
            later,
            SessionStatus::Ended,
            Some(now - Duration::days(9))
        )]
    );
}

#[test]
fn confirmed_prune_skips_missing_and_changed_candidates_without_partial_lies() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let removed = "tether-0197f198000070008000000000000011";
    let changed = "tether-0197f198000070008000000000000012";
    let missing = "tether-0197f198000070008000000000000013";
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                prune_record(removed, SessionStatus::Ended, Some(now - Duration::days(8))),
                prune_record(changed, SessionStatus::Ended, Some(now - Duration::days(8))),
                prune_record(missing, SessionStatus::Ended, Some(now - Duration::days(8))),
            ],
        })
        .unwrap();
    let service = PruneService::new(store.clone());
    let preview = service.preview_at(7, now).unwrap();
    store
        .update(|state| {
            state.sessions[1].directory = "/concurrently/changed".into();
            state.sessions.remove(2);
            Ok(())
        })
        .unwrap();

    let result = service.apply(&preview).unwrap();
    assert_eq!(result.removed_ids, vec![removed.parse().unwrap()]);
    assert_eq!(
        result.skipped_ids,
        vec![changed.parse().unwrap(), missing.parse().unwrap()]
    );
    assert_eq!(
        store.load().unwrap().sessions[0].directory,
        "/concurrently/changed"
    );
}

#[test]
fn prune_service_has_no_transport_configuration_or_capability() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let service = PruneService::new(store);
    let preview = service.preview(7).unwrap();
    assert!(preview.ids().is_empty());
    assert!(service.apply(&preview).unwrap().removed_ids.is_empty());
}

#[test]
fn prune_state_failures_are_typed_and_apply_does_not_rewrite_bad_state() {
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path.clone());
    store.save(&State::default()).unwrap();
    let service = PruneService::new(store);
    let preview = service.preview(7).unwrap();
    let invalid = b"{ definitely not state }\n";
    fs::write(&state_path, invalid).unwrap();

    assert!(matches!(service.apply(&preview), Err(PruneError::State(_))));
    assert_eq!(fs::read(state_path).unwrap(), invalid);
}

#[test]
fn ended_observation_persists_exit_context_and_exact_identity() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (temp, store, service, _) = lifecycle_fixture(
        "tether-0197f198000070008000000000000001:$7:0:1:130:0197f198000070008000000000000002\n",
    );

    assert_eq!(
        service.observe_owned(id()).unwrap(),
        WorkloadState::Ended {
            identity: "$7".parse().unwrap(),
            exit_status: Some(130),
        }
    );
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Ended);
    assert_eq!(record.tmux_session_id, Some("$7".parse().unwrap()));
    assert_eq!(record.exit_status, Some(130));
    assert_eq!(record.closed_at, Some(record.last_used_at));
    drop(temp);
}

#[test]
fn stop_refuses_replacement_incarnation_without_sending_kill() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let calls = temp.path().join("calls");
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1 $2 $3\" >> '{calls}'\ncase \"$1\" in\nlist-sessions)\n  count=$(grep -c list-sessions '{calls}')\n  if [ \"$count\" -eq 1 ]; then identity='$7'; else identity='$8'; fi\n  printf 'tether-0197f198000070008000000000000001:%s:0:0::0197f198000070008000000000000002' \"$identity\";;\nkill-session) exit 99;;\nesac\n",
        calls = calls.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
        })
        .unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    assert!(matches!(
        service.stop_owned(id()),
        Err(CloseOwnedError::ConcurrentModification(_))
    ));
    let calls = fs::read_to_string(calls).unwrap();
    assert!(!calls.contains("kill-session"));
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Stopping);
    assert_eq!(record.tmux_session_id, Some("$7".parse().unwrap()));
}

#[test]
fn stop_guard_rejects_identity_reuse_after_final_inspection() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let calls = temp.path().join("calls");
    let killed = temp.path().join("killed");
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{calls}'\ncase \"$1\" in\nlist-sessions) printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002';;\nif-shell) printf 'TETHER_OWNERSHIP_GUARD_REJECTED\\n';;\nkill-session) : > '{killed}';;\nesac\n",
        calls = calls.display(),
        killed = killed.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let mut record = owned_record(SessionStatus::Running);
    record.tmux_session_id = Some("$7".parse().unwrap());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![record],
        })
        .unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    assert!(matches!(
        service.stop_owned(id()),
        Err(CloseOwnedError::ConcurrentModification(_))
    ));
    assert!(
        !killed.exists(),
        "guard failure must not execute kill-session"
    );
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Stopping
    );
    let calls = fs::read_to_string(calls).unwrap();
    assert!(calls.lines().any(|call| call == "if-shell"));
    assert!(!calls.lines().any(|call| call == "kill-session"));
}

#[test]
fn creating_recovery_refuses_same_name_with_wrong_ownership_proof() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (_temp, store, service, log) = lifecycle_fixture(
        "tether-0197f198000070008000000000000001:$9:0:0::0197f198000070008000000000000099\n",
    );
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Creating;
            Ok(())
        })
        .unwrap();

    assert!(matches!(
        service.restart_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Creating);
    assert_eq!(record.tmux_session_id, None);
    assert!(
        !read_argv(&log)
            .iter()
            .any(|argument| argument == "new-session")
    );
    assert!(
        !read_argv(&log)
            .iter()
            .any(|argument| argument == "kill-session")
    );
}

#[test]
fn open_revalidates_proof_and_identity_at_mutation_boundary() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let calls = temp.path().join("calls");
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{calls}'\ncase \"$1\" in\nlist-sessions)\n  count=$(grep -c list-sessions '{calls}')\n  if [ \"$count\" -eq 1 ]; then proof='0197f198000070008000000000000002'; else proof='0197f198000070008000000000000099'; fi\n  printf 'tether-0197f198000070008000000000000001:$7:0:0::%s' \"$proof\";;\nattach-session) exit 99;;\nesac\n",
        calls = calls.display(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let mut record = owned_record(SessionStatus::Running);
    record.tmux_session_id = Some("$7".parse().unwrap());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![record],
        })
        .unwrap();
    let service = LifecycleService::new(
        store,
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );

    assert!(matches!(
        service.open_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    assert!(
        !fs::read_to_string(calls)
            .unwrap()
            .contains("attach-session")
    );
}

#[test]
fn restart_recovers_existing_creating_incarnation_idempotently() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (_temp, store, service, _) = lifecycle_fixture(
        "tether-0197f198000070008000000000000001:$9:0:0::0197f198000070008000000000000002\n",
    );
    store
        .update(|state| {
            state.sessions[0].status = SessionStatus::Creating;
            Ok(())
        })
        .unwrap();

    let first = service.restart_owned(id()).unwrap();
    let second = service.restart_owned(id()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.identity, "$9".parse().unwrap());
    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.status, SessionStatus::Running);
    assert_eq!(record.tmux_session_id, Some(first.identity));
    assert_eq!(record.command.as_deref(), Some("exec shell"));
}

#[test]
fn remove_never_kills_a_running_incarnation() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (_temp, store, service, log) = lifecycle_fixture(
        "tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002\n",
    );
    store
        .update(|state| {
            let record = &mut state.sessions[0];
            record.status = SessionStatus::Ended;
            record.closed_at = Some(record.last_used_at);
            record.tmux_session_id = Some("$7".parse().unwrap());
            Ok(())
        })
        .unwrap();

    assert!(matches!(
        service.remove_owned(id()),
        Err(CloseOwnedError::ConcurrentModification(_))
    ));
    assert!(
        !read_argv(&log)
            .iter()
            .any(|argument| argument == "kill-session")
    );
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Ended
    );
}

#[test]
fn automatic_retention_removes_only_finalized_metadata_without_transport() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let old = now - Duration::days(31);
    let mut removed = prune_record(
        "tether-0197f198000070008000000000000021",
        SessionStatus::Removed,
        Some(old),
    );
    removed.last_used_at = old;
    let ended = prune_record(
        "tether-0197f198000070008000000000000022",
        SessionStatus::Ended,
        Some(old),
    );
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![removed, ended],
        })
        .unwrap();

    let removed_ids = PruneService::new(store.clone())
        .automatic_cleanup_at(now)
        .unwrap();
    assert_eq!(
        removed_ids,
        vec!["tether-0197f198000070008000000000000021".parse().unwrap()]
    );
    assert_eq!(store.load().unwrap().sessions.len(), 1);
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Ended
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

#[cfg(unix)]
#[test]
fn direct_backend_inspection_rejects_oversized_process_output() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    fs::write(&tmux, "#!/bin/sh\nhead -c 70000 /dev/zero\nexit 0\n").unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let error = backend.inspect(&id(), &proof()).unwrap_err();
    assert!(error.to_string().contains("safe capture limit"));
}
