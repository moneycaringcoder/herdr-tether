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
    audit::{AuditAction, AuditStore},
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    lifecycle::{
        CleanupEligibility, CloseOwnedError, ClosedWorkload, LifecycleService, PruneError,
        PruneService, cleanup_eligibility,
    },
    model::{
        ExternalSessionName, OrchestrationGroupId, OrchestrationMembershipId, OrchestrationTitle,
        OwnershipProof, SessionId,
    },
    quote::posix_quote,
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, StateStore,
    },
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
        herdr_agent: None,
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
        immediate_failures: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
    // The end is recorded without moving the start stamp, so the record still
    // says how long this incarnation ran.
    assert!(record.closed_at.unwrap() > record.last_used_at);
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

/// A fake `tmux` that answers the exact inspection with `line` and lets the
/// close path succeed, which needs `if-shell` to exit cleanly with no output.
fn write_close_aware_fake(path: &Path, log: &Path, line: &str) {
    let script = format!(
        "#!/bin/sh\nfor arg do printf '%s\\000' \"$arg\" >> '{log}'; done\ncase \"$1\" in\nlist-sessions) printf '%s' '{line}';;\nesac\nexit 0\n",
        log = log.display(),
        line = line.replace('\'', "'\\''"),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn stop_fixture(line: &str) -> (tempfile::TempDir, StateStore, LifecycleService) {
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
            orchestration_groups: Vec::new(),
        })
        .unwrap();
    let tmux = temp.path().join("tmux");
    write_close_aware_fake(&tmux, &temp.path().join("tmux.args"), line);
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), tmux),
    );
    (temp, store, service)
}

#[test]
fn a_real_stop_leaves_both_of_its_transitions_in_the_trail() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    // A dead pane reporting status 2: a stop is two transitions, and the record
    // afterwards shows only the second one.
    let (temp, store, service) = stop_fixture(
        "tether-0197f198000070008000000000000001:$7:0:1:2:0197f198000070008000000000000002",
    );
    let trail = AuditStore::new(temp.path().join("audit.json"), 30);
    let service = service.with_audit(trail.clone());

    service.stop_owned(id()).unwrap();

    let entries = trail.entries().unwrap();
    let actions: Vec<AuditAction> = entries.iter().map(|entry| entry.action).collect();
    assert_eq!(
        actions,
        vec![AuditAction::Stop, AuditAction::Stopped],
        "a stop passes through Stopping on its way to Ended: {entries:?}"
    );
    assert_eq!(entries[0].from, Some(SessionStatus::Running));
    assert_eq!(entries[1].to, SessionStatus::Ended);
    assert_eq!(
        entries[1].exit_status,
        Some(2),
        "the trail keeps how it ended, not just that it ended"
    );
    assert_eq!(
        entries[1].session,
        store.load().unwrap().sessions[0].id,
        "the trail names the same workload the state does"
    );
}

#[test]
fn a_trail_that_cannot_be_written_does_not_fail_the_stop() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let (temp, store, service) = stop_fixture(
        "tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002",
    );
    // A directory where the trail expects a file: recording cannot succeed.
    let path = temp.path().join("audit.json");
    fs::create_dir(&path).unwrap();
    let service = service.with_audit(AuditStore::new(path, 30));

    service
        .stop_owned(id())
        .expect("a record of the work is not a precondition for the work");
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Ended
    );
}

#[test]
fn a_restart_keeps_the_failure_count_and_a_successful_run_clears_it() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let mut record = owned_record(SessionStatus::Running);
    // Started a second ago, so the end recorded below is an immediate failure.
    record.last_used_at = chrono::Utc::now() - chrono::TimeDelta::seconds(1);
    record.command = Some("exec false".to_owned());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![record],
            orchestration_groups: Vec::new(),
        })
        .unwrap();

    // A dead pane reporting a failing status: the shape of a loop.
    let tmux = temp.path().join("tmux");
    write_close_aware_fake(
        &tmux,
        &temp.path().join("tmux.args"),
        "tether-0197f198000070008000000000000001:$7:0:1:3:0197f198000070008000000000000002",
    );
    LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(temp.path().join("unused-ssh"), &tmux),
    )
    .stop_owned(id())
    .unwrap();
    let ended = &store.load().unwrap().sessions[0];
    assert_eq!(
        ended.immediate_failures.len(),
        1,
        "the end that just happened is recorded: {:?}",
        ended.immediate_failures
    );

    // A real restart whose creation fails. The reservation is what survives that,
    // and the count has to survive with it: it is the only evidence that a
    // subsequent failure is the second in a row, and clearing it here is exactly
    // what kept Tether from counting before.
    let failing_create = temp.path().join("failing-create-tmux");
    fs::write(
        &failing_create,
        "#!/bin/sh\ncase \"$1\" in\n  list-sessions) exit 0 ;;\n  new-session) exit 1 ;;\nesac\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&failing_create, fs::Permissions::from_mode(0o700)).unwrap();
    let restarting = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new("unused-ssh", &failing_create),
    );
    assert!(
        restarting.restart_owned(id()).is_err(),
        "the fake refuses to create, which is the case the reservation exists for"
    );
    let reserved = &store.load().unwrap().sessions[0];
    assert_eq!(reserved.status, SessionStatus::Creating);
    assert_eq!(
        reserved.immediate_failures.len(),
        1,
        "a reservation does not forget the failure it is retrying: {:?}",
        reserved.immediate_failures
    );

    // A restart that does start is not proof the loop ended: a looping workload
    // starts successfully every time, so a start that cleared the count would
    // hold it at one forever and the wait would never grow.
    let running = temp.path().join("running-tmux");
    write_fake(
        &running,
        &temp.path().join("running.args"),
        "tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002",
        0,
    );
    let restarted =
        LifecycleService::new(store.clone(), ProcessBinaries::new("unused-ssh", &running));
    restarted.restart_owned(id()).unwrap();
    let promoted = &store.load().unwrap().sessions[0];
    assert_eq!(promoted.status, SessionStatus::Running);
    assert_eq!(
        promoted.immediate_failures.len(),
        1,
        "a promotion is not evidence the loop is over: {:?}",
        promoted.immediate_failures
    );

    // Nor is observing it running afterwards.
    restarted.observe_owned(id()).unwrap();
    assert_eq!(
        store.load().unwrap().sessions[0].immediate_failures.len(),
        1,
        "an observation is not evidence either"
    );

    // An end of a different shape is that evidence. This one ran for a minute
    // before failing, which is a workload with a problem rather than one that
    // cannot start, so the run of immediate failures is over.
    let mut record = store.load().unwrap().sessions.remove(0);
    record.last_used_at = chrono::Utc::now() - chrono::TimeDelta::minutes(1);
    record.status = SessionStatus::Stopping;
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![record],
            orchestration_groups: Vec::new(),
        })
        .unwrap();
    LifecycleService::new(store.clone(), ProcessBinaries::new("unused-ssh", &tmux))
        .stop_owned(id())
        .unwrap();
    let cleared = &store.load().unwrap().sessions[0];
    assert!(
        cleared.immediate_failures.is_empty(),
        "an end that is not immediate ends the run: {:?}",
        cleared.immediate_failures
    );
}

#[test]
fn stopping_an_already_dead_workload_keeps_the_exit_status_it_observed() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    // A dead pane that `remain-on-exit` kept listed, reporting status 2. This is
    // the only moment that status exists anywhere: reaping the pane destroys it.
    let (_temp, store, service) = stop_fixture(
        "tether-0197f198000070008000000000000001:$7:0:1:2:0197f198000070008000000000000002",
    );

    let result = service.stop_owned(id()).unwrap();
    assert_eq!(result.workload, ClosedWorkload::Terminated);

    let state = store.load().unwrap();
    let record = &state.sessions[0];
    assert_eq!(record.status, SessionStatus::Ended);
    assert_eq!(
        record.exit_status,
        Some(2),
        "reaping a workload that already failed must not erase how it failed"
    );
}

#[test]
fn stopping_a_running_workload_records_no_exit_status() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    // Tether ended this one on purpose, so any status would describe the kill
    // rather than the work.
    let (_temp, store, service) = stop_fixture(
        "tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000002",
    );

    let result = service.stop_owned(id()).unwrap();
    assert_eq!(result.workload, ClosedWorkload::Terminated);

    let state = store.load().unwrap();
    assert_eq!(state.sessions[0].status, SessionStatus::Ended);
    assert_eq!(state.sessions[0].exit_status, None);
}

#[test]
fn a_recorded_end_keeps_the_lifetime_the_pace_rule_reads() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    // The pace rule asks how long the last incarnation ran, which is the
    // difference between the start stamp and the recorded end. A terminal write
    // that moved the start stamp to the end instant would make every failure
    // look like it arrived the instant the workload started.
    let (_temp, store, service) = stop_fixture(
        "tether-0197f198000070008000000000000001:$7:0:1:2:0197f198000070008000000000000002",
    );
    let started = store.load().unwrap().sessions[0].last_used_at;

    service.stop_owned(id()).unwrap();

    let record = &store.load().unwrap().sessions[0];
    assert_eq!(record.last_used_at, started, "the start stamp must survive");
    assert!(record.closed_at.unwrap() > started);
    // The fixture's workload starts in the recorded past, so this end reads as a
    // long run rather than an immediate failure. What matters is that the
    // lifetime is readable at all: a terminal write that moved the start stamp
    // to the end instant would make every failure look immediate, and this
    // record would then be paced.
    assert!(!record.failed_immediately(), "a long run is not a loop");
    let mut failed_at_once = record.clone();
    failed_at_once.last_used_at = record.closed_at.unwrap() - chrono::Duration::milliseconds(400);
    assert!(failed_at_once.failed_immediately());
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
                orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
                orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
fn owned_capture_uses_exact_guard_and_single_workload_pane_without_mutation() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    write_fake(&tmux, &log, "first\r\nsecond\u{1b}[31m\n", 0);
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let capture = backend
        .capture_owned(&id(), &proof(), "$7".parse().unwrap())
        .unwrap();

    assert_eq!(capture.text(), "first\r\nsecond\u{1b}[31m\n");
    let argv = read_argv(&log);
    assert_eq!(&argv[..5], ["if-shell", "-t", "$7", "-F", argv[4].as_str()]);
    assert!(argv[4].contains("#{session_id},$7"));
    assert!(argv[4].contains(&id().to_string()));
    assert!(argv[4].contains(&proof().to_string()));
    assert_eq!(argv[5], "capture-pane -p -J -S -200 -t $7");
    assert!(argv[6].contains("TETHER_OWNERSHIP_GUARD_REJECTED"));
    for forbidden in [
        "send-keys",
        "set-option",
        "kill-",
        "attach-session",
        "new-session",
    ] {
        assert!(!argv.iter().any(|arg| arg.contains(forbidden)), "{argv:?}");
    }
}

#[test]
fn owned_capture_does_not_confuse_worker_content_with_guard_rejection() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let log = temp.path().join("tmux.args");
    write_fake(&tmux, &log, "TETHER_OWNERSHIP_GUARD_REJECTED\n", 0);
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    assert_eq!(
        backend
            .capture_owned(&id(), &proof(), "$7".parse().unwrap())
            .unwrap()
            .text(),
        "TETHER_OWNERSHIP_GUARD_REJECTED\n"
    );
}

#[test]
fn remote_owned_capture_passes_one_fully_quoted_read_only_command() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let log = temp.path().join("ssh.args");
    write_fake(&ssh, &log, "remote output\n", 0);
    let backend = TmuxBackend::remote(
        "ssh://builder@example.test:2222",
        ProcessBinaries::new(ssh, temp.path().join("unused-tmux")),
    )
    .unwrap();

    assert_eq!(
        backend
            .capture_owned(&id(), &proof(), "$7".parse().unwrap())
            .unwrap()
            .into_text(),
        "remote output\n"
    );

    let argv = read_argv(&log);
    assert_eq!(
        &argv[..18],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPersist=60",
            "-o",
            "ControlPath=~/.ssh/herdr-tether-%C",
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
    assert_eq!(argv.len(), 19);
    let remote = &argv[18];
    assert!(remote.starts_with("'tmux' 'if-shell' '-t' '$7' '-F' "));
    assert!(remote.contains("'capture-pane -p -J -S -200 -t $7'"));
    assert!(remote.contains(&proof().to_string()));
    assert!(!remote.contains("send-keys"));
    assert!(!remote.contains("set-option"));
}

#[test]
fn owned_capture_rejects_stale_identity_name_or_proof_and_redacts_proof() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\ncase \"$5\" in *'#{{session_id}},$7'*'{id}'*'{proof}'*) printf owned;; *) printf 'TETHER_OWNERSHIP_GUARD_REJECTED\\n'; exit 75;; esac\n",
        id = id(),
        proof = proof(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    assert_eq!(
        backend
            .capture_owned(&id(), &proof(), "$7".parse().unwrap())
            .unwrap()
            .text(),
        "owned"
    );
    let stale_name = "tether-0197f198000070008000000000000009".parse().unwrap();
    let stale_proof = "0197f198000070008000000000000009".parse().unwrap();
    for error in [
        backend
            .capture_owned(&stale_name, &proof(), "$7".parse().unwrap())
            .unwrap_err(),
        backend
            .capture_owned(&id(), &stale_proof, "$7".parse().unwrap())
            .unwrap_err(),
        backend
            .capture_owned(&id(), &proof(), "$8".parse().unwrap())
            .unwrap_err(),
    ] {
        assert_eq!(
            error.to_string(),
            "exact owned tmux identity changed before capture"
        );
        assert!(!error.to_string().contains(&proof().to_string()));
        assert!(!error.to_string().contains(&stale_proof.to_string()));
    }
}

#[cfg(unix)]
#[test]
fn owned_capture_rejects_oversized_output() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    fs::write(&tmux, "#!/bin/sh\nhead -c 70000 /dev/zero\n").unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let error = backend
        .capture_owned(&id(), &proof(), "$7".parse().unwrap())
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "tmux capture exceeded the safe output limit"
    );
}

#[test]
fn owned_capture_transport_error_redacts_proof_and_terminal_controls() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let script = format!(
        "#!/bin/sh\nprintf '\\033]2;spoofed title\\007\\033[31mTETHER_OWNERSHIP_PROOF={proof}\\033[0m\\n' >&2\nexit 23\n",
        proof = proof(),
    );
    fs::write(&tmux, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let error = backend
        .capture_owned(&id(), &proof(), "$7".parse().unwrap())
        .unwrap_err()
        .to_string();
    assert!(error.contains("23"), "{error}");
    assert!(!error.contains(&proof().to_string()), "{error}");
    assert!(!error.contains("TETHER_OWNERSHIP_PROOF"), "{error}");
    assert!(!error.contains('\u{1b}'), "{error:?}");
    assert!(!error.contains("spoofed title"), "{error}");
}

#[cfg(unix)]
#[test]
fn owned_capture_times_out_and_cleans_its_process_group() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let tmux = temp.path().join("tmux");
    let orphan = temp.path().join("orphan");
    let script = format!(
        "#!/bin/sh\n(sleep 31; printf orphan > '{}') &\nwait\n",
        orphan.display()
    );
    fs::write(&tmux, script).unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

    let error = backend
        .capture_owned(&id(), &proof(), "$7".parse().unwrap())
        .unwrap_err();
    assert_eq!(error.to_string(), "tmux capture timed out");
    thread::sleep(StdDuration::from_secs(2));
    assert!(!orphan.exists(), "timed-out capture left a child process");
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
        &argv[..18],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPersist=60",
            "-o",
            "ControlPath=~/.ssh/herdr-tether-%C",
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
        19,
        "ssh must receive one remote command argument"
    );
    let launch_script = "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" && exec /bin/sh -c \"$2\"";
    let remote = &argv[18];
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
        &command.args[..19],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPersist=60",
            "-o",
            "ControlPath=~/.ssh/herdr-tether-%C",
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
    assert_eq!(command.args[19], "'tmux' 'attach-session' '-t' '=work'");
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
fn a_stop_refuses_when_the_inspection_could_not_run() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path.clone());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
            orchestration_groups: Vec::new(),
        })
        .unwrap();
    let before = fs::read_to_string(&state_path).unwrap();

    // `tmux` is not installed.
    let absent = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new("unused-ssh", "tmux-that-does-not-exist-for-test"),
    );
    assert!(
        matches!(
            absent.stop_owned(id()),
            Err(CloseOwnedError::Inspect { .. })
        ),
        "a stop that could not look must not report having stopped anything"
    );

    // `tmux` is installed but cannot be executed.
    let dud = temp.path().join("dud-tmux");
    fs::write(&dud, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&dud, fs::Permissions::from_mode(0o000)).unwrap();
    let unusable = LifecycleService::new(store.clone(), ProcessBinaries::new("unused-ssh", &dud));
    assert!(matches!(
        unusable.stop_owned(id()),
        Err(CloseOwnedError::Inspect { .. })
    ));

    // `tmux` ran but rejected the query - an installation too old for the filter
    // this inspection needs. That is a failure to ask, not an answer.
    let rejecting = temp.path().join("rejecting-tmux");
    fs::write(
        &rejecting,
        "#!/bin/sh\necho 'unknown option -- f' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&rejecting, fs::Permissions::from_mode(0o700)).unwrap();
    let rejected = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new("unused-ssh", &rejecting),
    );
    assert!(
        matches!(
            rejected.stop_owned(id()),
            Err(CloseOwnedError::WorkloadUnknown(_))
        ),
        "a rejected query is not a workload that ended"
    );

    assert_eq!(
        fs::read_to_string(&state_path).unwrap(),
        before,
        "no refusal may leave the record changed"
    );

    // The distinction that makes the refusals meaningful. Both of these are
    // answers, and both end the record: a server that answered and holds no such
    // session, and no server on the socket at all - the ordinary state of a host
    // whose last session ended, which has to stay actionable.
    for reply in [
        "#!/bin/sh\nexit 0\n",
        "#!/bin/sh\necho 'no server running on /tmp/x' >&2\nexit 1\n",
    ] {
        store
            .save(&State {
                version: State::CURRENT_VERSION,
                sessions: vec![owned_record(SessionStatus::Running)],
                orchestration_groups: Vec::new(),
            })
            .unwrap();
        let answering = temp.path().join("answering-tmux");
        fs::write(&answering, reply).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&answering, fs::Permissions::from_mode(0o700)).unwrap();
        let reachable = LifecycleService::new(
            store.clone(),
            ProcessBinaries::new("unused-ssh", &answering),
        );
        assert_eq!(
            reachable.stop_owned(id()).unwrap().workload,
            ClosedWorkload::Missing
        );
        assert_eq!(
            store.load().unwrap().sessions[0].status,
            SessionStatus::Ended,
            "a record must not be left unactionable: {reply}"
        );
    }
}

#[test]
fn a_restart_and_an_observation_refuse_the_same_way() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    let temp = tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let store = StateStore::new(state_path.clone());
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![owned_record(SessionStatus::Running)],
            orchestration_groups: Vec::new(),
        })
        .unwrap();
    let before = fs::read_to_string(&state_path).unwrap();
    let rejecting = temp.path().join("rejecting-tmux");
    fs::write(
        &rejecting,
        "#!/bin/sh\necho 'unknown option -- f' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&rejecting, fs::Permissions::from_mode(0o700)).unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new("unused-ssh", &rejecting),
    );

    // A restart must not create a second incarnation of a workload it could not
    // ask about, and an observation must not reconcile one to ended.
    assert!(matches!(
        service.restart_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    assert!(matches!(
        service.observe_owned(id()),
        Err(CloseOwnedError::WorkloadUnknown(_))
    ));
    assert_eq!(fs::read_to_string(&state_path).unwrap(), before);
}

#[test]
fn inspect_separates_an_answer_from_a_failure_to_ask() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    for (status, stderr, expected) in [
        // A server that answered, holding no session matching the name and the
        // ownership proof: the query filters, so this is authoritative absence.
        (0, "", WorkloadState::Missing),
        // No server on this socket. The ordinary state of a host whose last
        // session ended, so it has to stay evidence: anything else would leave
        // every record on a rebooted machine impossible to act on.
        (
            1,
            "no server running on /tmp/tmux-1000/default",
            WorkloadState::Missing,
        ),
        (
            1,
            "error connecting to /tmp/tmux-1000/default (No such file or directory)",
            WorkloadState::Missing,
        ),
        (1, "", WorkloadState::Missing),
        // Exit 1 for any other reason is a failure to ask. A `tmux` too old for
        // the filter this query needs must not read as an absent workload.
        (1, "unknown option -- f", WorkloadState::Unknown),
        (
            1,
            "usage: list-sessions [-F format]",
            WorkloadState::Unknown,
        ),
        (2, "", WorkloadState::Unknown),
        (255, "", WorkloadState::Unknown),
    ] {
        let temp = tempdir().unwrap();
        let tmux = temp.path().join("tmux");
        fs::write(
            &tmux,
            format!("#!/bin/sh\nprintf '%s' '{stderr}' >&2\nexit {status}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let backend =
            TmuxBackend::local(ProcessBinaries::new(temp.path().join("unused-ssh"), tmux));

        assert_eq!(
            backend.inspect(&id(), &proof()).unwrap(),
            expected,
            "unexpected inspect state for exit {status} saying {stderr:?}"
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
        herdr_agent: None,
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
        immediate_failures: Vec::new(),
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
        herdr_agent: None,
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
        immediate_failures: Vec::new(),
    }
}

fn orchestration_group(
    id: &str,
    orchestrator_session_id: SessionId,
    workers: &[SessionId],
) -> OrchestrationGroup {
    OrchestrationGroup {
        id: id.parse::<OrchestrationGroupId>().unwrap(),
        title: format!("Group {id}").parse::<OrchestrationTitle>().unwrap(),
        orchestrator_session_id,
        workers: workers
            .iter()
            .copied()
            .map(|session_id| OrchestrationMember {
                session_id,
                membership_id: OrchestrationMembershipId::new(),
                title: Some("Stable worker title".parse().unwrap()),
                capabilities: OrchestrationCapabilities {
                    observe_output: true,
                    open_interactive: true,
                    prompt_agent: false,
                },
            })
            .collect(),
    }
}

#[test]
fn confirmed_prune_reconciles_worker_and_orchestrator_metadata_only() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let pruned_worker = "tether-0197f198000070008000000000000011";
    let pruned_orchestrator = "tether-0197f198000070008000000000000012";
    let stable_orchestrator = "tether-0197f198000070008000000000000013";
    let stable_worker = "tether-0197f198000070008000000000000014";
    let doomed_group = orchestration_group(
        "doomed",
        pruned_orchestrator.parse().unwrap(),
        &[stable_worker.parse().unwrap()],
    );
    let reconciled_group = orchestration_group(
        "reconciled",
        stable_orchestrator.parse().unwrap(),
        &[
            pruned_worker.parse().unwrap(),
            stable_worker.parse().unwrap(),
        ],
    );
    let unrelated_group = orchestration_group(
        "unrelated",
        stable_worker.parse().unwrap(),
        &[stable_orchestrator.parse().unwrap()],
    );
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                prune_record(
                    pruned_worker,
                    SessionStatus::Removed,
                    Some(now - Duration::days(8)),
                ),
                prune_record(
                    pruned_orchestrator,
                    SessionStatus::Removed,
                    Some(now - Duration::days(8)),
                ),
                prune_record(stable_orchestrator, SessionStatus::Running, None),
                prune_record(stable_worker, SessionStatus::Running, None),
            ],
            orchestration_groups: vec![
                doomed_group,
                reconciled_group.clone(),
                unrelated_group.clone(),
            ],
        })
        .unwrap();
    let service = PruneService::new(store.clone());
    let preview = service.preview_at(7, now).unwrap();

    assert_eq!(service.apply(&preview).unwrap().removed_ids.len(), 2);
    let state = store.load().unwrap();
    let mut expected_reconciled = reconciled_group;
    expected_reconciled
        .workers
        .retain(|worker| worker.session_id != pruned_worker.parse().unwrap());
    assert_eq!(
        state.orchestration_groups,
        vec![expected_reconciled, unrelated_group],
        "orchestrator deletion removes its group while worker deletion changes only membership"
    );
    assert_eq!(
        state
            .sessions
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![
            stable_orchestrator.parse().unwrap(),
            stable_worker.parse().unwrap()
        ]
    );
}

#[test]
fn metadata_only_remove_reconciles_group_without_backend_or_session_side_effects() {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let removed = id();
    let stable_orchestrator = "tether-0197f198000070008000000000000013".parse().unwrap();
    let stable_worker = "tether-0197f198000070008000000000000014".parse().unwrap();
    let mut legacy = owned_record(SessionStatus::Ended);
    legacy.ownership_proof = None;
    let worker_group = orchestration_group(
        "worker-removal",
        stable_orchestrator,
        &[removed, stable_worker],
    );
    let orchestrator_group = orchestration_group("orchestrator-removal", removed, &[stable_worker]);
    let unrelated_group =
        orchestration_group("unrelated-removal", stable_worker, &[stable_orchestrator]);
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions: vec![
                legacy,
                prune_record(
                    &stable_orchestrator.to_string(),
                    SessionStatus::Running,
                    None,
                ),
                prune_record(&stable_worker.to_string(), SessionStatus::Running, None),
            ],
            orchestration_groups: vec![
                worker_group.clone(),
                orchestrator_group,
                unrelated_group.clone(),
            ],
        })
        .unwrap();
    let service = LifecycleService::new(
        store.clone(),
        ProcessBinaries::new(
            temp.path().join("must-not-run-ssh"),
            temp.path().join("must-not-run-tmux"),
        ),
    );

    assert_eq!(
        service.remove_owned(removed).unwrap().workload,
        ClosedWorkload::Missing
    );
    let state = store.load().unwrap();
    let mut expected_worker_group = worker_group;
    expected_worker_group
        .workers
        .retain(|worker| worker.session_id != removed);
    assert_eq!(
        state.orchestration_groups,
        vec![expected_worker_group, unrelated_group]
    );
    assert_eq!(state.sessions[0].status, SessionStatus::Removed);
    assert_eq!(state.sessions[1].id, stable_orchestrator);
    assert_eq!(state.sessions[2].id, stable_worker);
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
fn re_observing_an_ended_workload_adds_nothing_to_the_trail() {
    let _guard = FAKE_PROCESS_LOCK.lock();
    // An empty catalog: the picker's status refresh reports every ended workload
    // as missing, and it does this on every refresh.
    let (temp, store, service, _) = lifecycle_fixture("\n");
    let trail = AuditStore::new(temp.path().join("audit.json"), 30);
    let service = service.with_audit(trail.clone());

    service.observe_owned(id()).unwrap();
    assert_eq!(
        store.load().unwrap().sessions[0].status,
        SessionStatus::Ended
    );
    let after_first = trail.entries().unwrap();
    assert_eq!(
        after_first.len(),
        1,
        "the reconciliation that ended it is worth a line: {after_first:?}"
    );

    for _ in 0..5 {
        service.observe_owned(id()).unwrap();
    }
    assert_eq!(
        trail.entries().unwrap(),
        after_first,
        "confirming what the record already said is not a transition; recording it \
         would evict the real history through the entry ceiling"
    );
    drop(temp);
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
    assert!(record.closed_at.unwrap() > record.last_used_at);
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
            orchestration_groups: Vec::new(),
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
