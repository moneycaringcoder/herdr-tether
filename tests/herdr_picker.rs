use std::{fs, io::Write, path::Path, sync::Mutex, time::SystemTime};

#[cfg(unix)]
use std::io::BufRead;

use chrono::{Duration, TimeZone, Utc};
use herdr_tether::{
    backend::{CommandSpec, ProcessBinaries},
    config::{CommandPreset, Config, DiscoveryDefaults, HostConfig, RetentionDefaults, UiDefaults},
    discovery::{DiscoveryCompletion, DiscoveryMessage},
    herdr::{HerdrClient, HerdrContext, InvocationLocation, PaneTitle},
    herdr_socket::HerdrSocketClient,
    lifecycle::{CloseOwnedError, PrunePreview, PruneService},
    model::{ExternalSessionName, OrchestrationGroupId, Placement, SessionId},
    state::{SessionRecord, SessionStatus, State, StateStore},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, MAX_STATUS_WORKLOADS,
        StatusMessage, StatusRequestError, StatusService, WorkloadStatus,
    },
    tui::{
        PickerCloseAction, PickerCloseModal, PickerCloseResult, PickerEvent, PickerHostOrigin,
        PickerInput, PickerOptions, PickerOutcome, PickerPruneModal, PickerPrunePhase,
        PickerPruneResult, PickerSelection, PickerStage, PickerState, format_close_error,
        render_picker_to_text,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static FAKE_HERDR_LOCK: Mutex<()> = Mutex::new(());

fn write_fake_herdr(path: &Path, log: &Path, pane_run: &str) {
    let script = format!(
        r#"#!/bin/sh
printf 'CALL' >> '{log}'
for arg do printf '\t%s' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
if [ "$1" = "--version" ]; then
  printf '%s\n' 'herdr 0.8.0'
  exit 0
fi
if [ "$1 $2" = "pane split" ]; then
  printf '%s' '{{"id":"cli-1","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9","workspace_id":"w1","tab_id":"w1:t1"}}}}}}'
elif [ "$1 $2" = "tab create" ]; then
  printf '%s' '{{"id":"cli-2","result":{{"type":"tab_created","tab":{{"tab_id":"w1:t9","workspace_id":"w1"}},"root_pane":{{"pane_id":"w1:p10","workspace_id":"w1","tab_id":"w1:t9"}}}}}}'
elif [ "$1 $2" = "pane rename" ]; then
  printf '%s' '{{"id":"rename","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9"}}}}}}'
elif [ "$1 $2" = "pane run" ]; then
  {pane_run}
elif [ "$1 $2" = "pane report-metadata" ]; then
  printf '%s' '{{"id":"metadata","result":{{"type":"ok"}}}}'
elif [ "$1 $2 $3" = "plugin pane open" ]; then
  printf '%s' '{{"id":"cli-4","result":{{"type":"ok"}}}}'
else
  printf '%s' '{{"id":"bad","error":{{"message":"unexpected fake invocation"}}}}'
  exit 2
fi
"#,
        log = log.display(),
        pane_run = pane_run,
    );
    let mut file = fs::File::create(path).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_fake_rename_failure_herdr(path: &Path, log: &Path) {
    let script = format!(
        r#"#!/bin/sh
printf 'CALL' >> '{log}'
for arg do printf '\t%s' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
if [ "$1 $2" = "pane split" ]; then
  printf '%s' '{{"id":"split","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9"}}}}}}'
elif [ "$1 $2" = "pane rename" ]; then
  printf '%s' '{{"id":"rename","error":{{"message":"titles unavailable"}}}}'
elif [ "$1 $2" = "pane run" ]; then
  printf '%s' '{{"id":"run","result":{{"type":"pane_ran","pane_id":"w1:p9"}}}}'
fi
"#,
        log = log.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

const CLOSE_OK: &str = r#"printf '%s' '{"id":"close","result":{"type":"ok"}}'"#;

fn write_fake_replace_herdr(
    path: &Path,
    log: &Path,
    destination_processes: &str,
    close_result: &str,
) {
    let script = format!(
        r#"#!/bin/sh
printf 'CALL' >> '{log}'
for arg do printf '\t%s' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
if [ "$1 $2 $3" = "pane process-info --pane" ]; then
  if [ "$4" = "w1:p1" ]; then
    printf '%s' '{{"id":"source","result":{{"type":"pane_process_info","process_info":{{"pane_id":"w1:p1","shell_pid":101,"foreground_processes":[{{"pid":202,"name":"vim","argv":["vim","notes.txt"],"cwd":"/tmp"}}]}}}}}}'
  else
    {destination_processes}
  fi
elif [ "$1 $2" = "pane split" ]; then
  printf '%s' '{{"id":"split","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9","workspace_id":"w1","tab_id":"w1:t1"}}}}}}'
elif [ "$1 $2" = "pane rename" ]; then
  printf '%s' '{{"id":"rename","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9"}}}}}}'
elif [ "$1 $2" = "pane run" ]; then
  printf '%s' '{{"id":"run","result":{{"type":"pane_ran","pane_id":"w1:p9"}}}}'
elif [ "$1 $2" = "pane close" ]; then
  {close_result}
else
  printf '%s' '{{"id":"bad","error":{{"message":"unexpected fake invocation"}}}}'
  exit 2
fi
"#,
        log = log.display(),
        destination_processes = destination_processes,
        close_result = close_result,
    );
    let mut file = fs::File::create(path).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_fake_reused_source_herdr(path: &Path, log: &Path) {
    let script = format!(
        r#"#!/bin/sh
printf 'CALL' >> '{log}'
for arg do printf '\t%s' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
if [ "$1 $2 $3" = "pane process-info --pane" ]; then
  if [ "$4" = "w1:p1" ]; then
    count=$(grep -c 'process-info.*w1:p1' '{log}')
    if [ "$count" -eq 1 ]; then pid=202; name=vim; else pid=909; name=replacement; fi
    printf '{{"id":"source","result":{{"type":"pane_process_info","process_info":{{"pane_id":"w1:p1","foreground_processes":[{{"pid":%s,"name":"%s","argv":["%s"]}}]}}}}}}' "$pid" "$name" "$name"
  else
    printf '%s' '{{"id":"destination","result":{{"type":"pane_process_info","process_info":{{"pane_id":"w1:p9","foreground_processes":[{{"pid":404,"name":"tmux","argv":["/usr/bin/tmux","attach-session","-t","$7"]}}]}}}}}}'
  fi
elif [ "$1 $2" = "pane split" ]; then
  printf '%s' '{{"id":"split","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9"}}}}}}'
elif [ "$1 $2" = "pane rename" ]; then
  printf '%s' '{{"id":"rename","result":{{"type":"pane_info","pane":{{"pane_id":"w1:p9"}}}}}}'
elif [ "$1 $2" = "pane run" ]; then
  printf '%s' '{{"id":"run","result":{{"type":"pane_ran","pane_id":"w1:p9"}}}}'
elif [ "$1 $2" = "pane close" ]; then
  printf '%s' '{{"id":"close","result":{{"type":"ok"}}}}'
fi
"#,
        log = log.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_fake_tmux_for_open(path: &Path, state: &Path) {
    let script = format!(
        r#"#!/bin/sh
command=$1
shift
case "$command" in
  new-session)
    previous=
    for arg do
      if [ "$previous" = '-s' ]; then printf '%s' "$arg" > '{id}'; fi
      if [ "$previous" = '-c' ]; then printf '%s' "$arg" > '{cwd}'; fi
      case "$arg" in TETHER_OWNERSHIP_PROOF=*) printf '%s' "${{arg#*=}}" > '{proof}' ;; esac
      previous=$arg
    done
    printf '$7:%%3'
    ;;
  list-sessions)
    id=$(cat '{id}' 2>/dev/null)
    proof=$(cat '{proof}' 2>/dev/null)
    case "$*" in
      *TETHER_OWNERSHIP_PROOF*) [ -n "$id" ] && printf '%s:$7:0:0::%s' "$id" "$proof" ;;
      *) [ -n "$id" ] && printf '%s:$7' "$id" ;;
    esac
    ;;
  display-message) cat '{cwd}' 2>/dev/null ;;
esac
"#,
        id = state.with_extension("id").display(),
        proof = state.with_extension("proof").display(),
        cwd = state.with_extension("cwd").display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_fake_ssh_for_open(path: &Path, state: &Path) {
    let script = format!(
        r#"#!/bin/sh
for remote do :; done
case "$remote" in
  *"'new-session'"*)
    value=${{remote#*"'new-session' '-d' '-s' '"}}
    printf '%s' "${{value%%\'*}}" > '{id}'
    value=${{remote#*"'TETHER_OWNERSHIP_PROOF="}}
    printf '%s' "${{value%%\'*}}" > '{proof}'
    value=${{remote#*"'-c' '"}}
    printf '%s' "${{value%%\'*}}" > '{cwd}'
    printf '$7:%%3'
    ;;
  *"'list-sessions'"*)
    id=$(cat '{id}' 2>/dev/null)
    proof=$(cat '{proof}' 2>/dev/null)
    case "$remote" in
      *TETHER_OWNERSHIP_PROOF*) [ -n "$id" ] && printf '%s:$7:0:0::%s' "$id" "$proof" ;;
      *) [ -n "$id" ] && printf '%s:$7' "$id" ;;
    esac
    ;;
  *"'display-message'"*) cat '{cwd}' 2>/dev/null ;;
esac
"#,
        id = state.with_extension("id").display(),
        proof = state.with_extension("proof").display(),
        cwd = state.with_extension("cwd").display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run_real_open(
    temp: &Path,
    herdr: &Path,
    path: &std::ffi::OsStr,
    arguments: &[&str],
) -> std::process::Output {
    let home = temp.join("home");
    let config = temp.join("config");
    let state = temp.join("state");
    fs::create_dir_all(&home).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_herdr-tether"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        .env("PATH", path)
        .env("HERDR_BIN_PATH", herdr)
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .args(arguments)
        .output()
        .unwrap()
}

fn context(binary: &Path) -> HerdrContext {
    HerdrContext {
        binary: binary.into(),
        pane_id: "w1:p1".into(),
        workspace_id: "w1".into(),
    }
}

#[test]
fn real_new_open_callsites_supply_owned_workload_titles() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    let transcript = temp.path().join("herdr.log");
    write_fake_herdr(
        &herdr,
        &transcript,
        r#"printf '%s' '{"id":"run","result":{"type":"pane_ran","pane_id":"w1:p9"}}'"#,
    );
    write_fake_tmux_for_open(&bin.join("tmux"), &temp.path().join("tmux-state"));
    write_fake_ssh_for_open(&bin.join("ssh"), &temp.path().join("ssh-state"));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();

    let config = Config {
        hosts: vec![HostConfig {
            name: "build-box".into(),
            target: "builder@example.test".into(),
            roots: vec!["/srv".into()],
            presets: Vec::new(),
        }],
        ..Config::default()
    };
    let config_file = temp.path().join("config/herdr-tether/config.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(config_file, toml::to_string_pretty(&config).unwrap()).unwrap();

    for arguments in [
        [
            "open",
            "--host",
            "local",
            "--directory",
            "/work/project",
            "--command",
            "exec /opt/agents/codex --quiet",
            "--placement",
            "split-right",
        ],
        [
            "open",
            "--host",
            "build-box",
            "--directory",
            "/srv/monorepo",
            "--command",
            "exec /opt/agents/claude --resume",
            "--placement",
            "split-right",
        ],
    ] {
        let output = run_real_open(temp.path(), &herdr, &path, &arguments);
        assert!(
            output.status.success(),
            "open failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let transcript = fs::read_to_string(transcript).unwrap();
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\tproject · codex"));
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\tbuild-box · monorepo · claude"));
    assert!(
        transcript
            .lines()
            .filter(|line| line.starts_with("CALL\tpane\trename"))
            .all(|line| !line.contains("builder@example.test"))
    );
}

#[test]
fn placement_parses_returned_ids_and_runs_one_quoted_command_argument() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let client = HerdrClient::new(context(&binary));
    let command = CommandSpec {
        program: "/tmp/plugin root/herdr-tether".into(),
        args: vec![
            "session".into(),
            "resume".into(),
            "tether-0197f198000070008000000000000001".into(),
        ],
    };

    let right = client
        .place(&command, &PaneTitle::fallback(), Placement::SplitRight)
        .unwrap();
    assert_eq!(right.pane_id, "w1:p9");
    let down = client
        .place(&command, &PaneTitle::fallback(), Placement::SplitDown)
        .unwrap();
    assert_eq!(down.pane_id, "w1:p9");
    let tab = client
        .place(&command, &PaneTitle::fallback(), Placement::NewTab)
        .unwrap();
    assert_eq!(tab.pane_id, "w1:p10");

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("CALL\tpane\tsplit\t--pane\tw1:p1\t--direction\tright\t--focus"));
    assert!(transcript.contains("CALL\tpane\tsplit\t--pane\tw1:p1\t--direction\tdown\t--focus"));
    assert!(transcript.contains("CALL\ttab\tcreate\t--workspace\tw1\t--focus"));
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\tTether session"));
    assert!(transcript.contains("CALL\tpane\trename\tw1:p10\tTether session"));
    assert!(transcript.contains("CALL\tpane\trun\tw1:p9\t'env' '-u' 'HERDR_BIN_PATH'"));
    assert!(transcript.contains(
        "'/tmp/plugin root/herdr-tether' 'session' 'resume' 'tether-0197f198000070008000000000000001'"
    ));
}

#[test]
fn placement_builder_receives_exact_destination_context_before_command_is_run() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let client = HerdrClient::new(context(&binary));

    let placed = client
        .place_with_destination(&PaneTitle::fallback(), Placement::NewTab, |destination| {
            Ok(CommandSpec::new(
                "/plugin/herdr-tether",
                vec![
                    "orchestration".into(),
                    "observer-runtime".into(),
                    "--pane-id".into(),
                    destination.pane_id.clone(),
                    "--workspace-id".into(),
                    destination.workspace_id.clone(),
                    "--herdr-bin".into(),
                    destination.binary.display().to_string(),
                ],
            ))
        })
        .unwrap();

    assert_eq!(placed.pane_id, "w1:p10");
    let transcript = fs::read_to_string(log).unwrap();
    assert_eq!(
        transcript
            .lines()
            .filter(|line| line.starts_with("CALL\tpane\trun"))
            .count(),
        1
    );
    assert!(transcript.contains("CALL\ttab\tcreate\t--workspace\tw1\t--focus"));
    assert!(
        transcript.contains(
            "'observer-runtime' '--pane-id' 'w1:p10' '--workspace-id' 'w1' '--herdr-bin'"
        )
    );
    assert!(!transcript.contains("'--pane-id' 'w1:p1'"));
}

#[test]
fn orchestration_observe_creates_one_outer_pane_with_exact_destination_context() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let state_file = temp.path().join("state/herdr-tether/state.json");
    fs::create_dir_all(state_file.parent().unwrap()).unwrap();
    fs::write(
        state_file,
        r#"{
  "version": 4,
  "sessions": [],
  "orchestration_groups": [{
    "id": "build-fleet",
    "title": "Build fleet",
    "orchestrator_session_id": "tether-0197f198000070008000000000000001",
    "workers": []
  }]
}"#,
    )
    .unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();

    let output = run_real_open(
        temp.path(),
        &binary,
        &path,
        &[
            "orchestration",
            "observe",
            "build-fleet",
            "--placement",
            "replace-current-pane",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript = fs::read_to_string(log).unwrap();
    assert_eq!(
        transcript
            .lines()
            .filter(|line| line.starts_with("CALL\tpane\tsplit"))
            .count(),
        1
    );
    assert!(transcript.contains("CALL\tpane\tsplit\t--pane\tw1:p1\t--direction\tright\t--focus"));
    assert!(
        !transcript.contains("CALL\tpane\tclose"),
        "Observer launch must preserve its source pane: {transcript}"
    );
    assert_eq!(
        transcript
            .lines()
            .filter(|line| line.starts_with("CALL\tpane\trun"))
            .count(),
        1
    );
    assert!(
        !transcript
            .lines()
            .any(|line| line.starts_with("CALL\tpane\trun\tw1:p1\t")),
        "Observer launch must not replace the source shell command: {transcript}"
    );
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\tObserver · Build fleet"));
    assert!(transcript.contains(
        "'observer-runtime' 'build-fleet' '--pane-id' 'w1:p9' '--workspace-id' 'w1' '--herdr-bin'"
    ));
}
#[test]
fn placement_titles_use_available_context_and_bound_hostile_input() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let client = HerdrClient::new(context(&binary));

    let cases = [
        PaneTitle::owned(
            "local",
            "/srv/repositories/tether",
            None,
            Some("codex --cd ."),
        ),
        PaneTitle::owned("local", "/", None, Some("codex")),
        PaneTitle::owned("local", "/srv/仓库", None, Some("codex")),
        PaneTitle::owned(
            "dev@example.test",
            "/srv/repository",
            Some("review"),
            Some("ignored-secret --token=hunter2"),
        ),
        PaneTitle::external("local", "workspace"),
        PaneTitle::external("build-box", "repository"),
        PaneTitle::external(
            "bad\t|·\u{202e}host",
            &format!("agent\u{200b}\n{}", "x".repeat(80)),
        ),
        PaneTitle::owned("\t|·", "\t|·", Some("\t|·"), Some("\t|·")),
        PaneTitle::fallback(),
    ];
    for title in &cases {
        client
            .place(
                &CommandSpec::new("/opaque/herdr-tether", vec!["attach".into()]),
                title,
                Placement::SplitRight,
            )
            .unwrap();
    }

    let transcript = fs::read_to_string(log).unwrap();
    let titles = transcript
        .lines()
        .filter_map(|line| line.strip_prefix("CALL\tpane\trename\tw1:p9\t"))
        .collect::<Vec<_>>();
    assert_eq!(titles[0], "tether · codex");
    assert_eq!(titles[1], "/ · codex");
    assert_eq!(titles[2], "仓库 · codex");
    assert_eq!(titles[3], "dev@example.test · repository · review");
    assert_eq!(titles[4], "workspace");
    assert_eq!(titles[5], "build-box · repository");
    assert_eq!(titles[7], "Tether session");
    assert_eq!(titles[8], "Tether session");
    assert_eq!(titles.len(), 9);
    assert!(!titles[6].chars().any(char::is_control));
    assert!(!titles[6].contains('|'));
    assert!(!titles[6].contains(['\u{202e}', '\u{200b}']));
    assert!(titles[6].chars().count() <= 48);
    assert!(titles[6].ends_with('…'));
}

#[test]
fn placement_runs_in_the_created_pane_when_rename_fails() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_rename_failure_herdr(&binary, &log);

    let placed = HerdrClient::new(context(&binary))
        .place(
            &CommandSpec::new("codex", vec!["--cd".into(), "/srv/repository".into()]),
            &PaneTitle::owned("local", "/srv/repository", None, Some("codex")),
            Placement::SplitRight,
        )
        .unwrap();

    assert_eq!(placed.pane_id, "w1:p9");
    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\trepository · codex"));
    assert!(transcript.contains("CALL\tpane\trun\tw1:p9"));
}

#[test]
fn placement_rejects_failed_or_mismatched_pane_run() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (pane_run, expected) in [
        ("exit 9", "failed with status"),
        (
            r#"printf '%s' '{"id":"cli-3","result":{"type":"pane_ran","pane_id":"w1:pX"}}'"#,
            "not newly created pane",
        ),
    ] {
        let temp = tempdir().unwrap();
        let binary = temp.path().join("herdr");
        let log = temp.path().join("herdr.log");
        write_fake_herdr(&binary, &log, pane_run);
        let error = HerdrClient::new(context(&binary))
            .place(
                &CommandSpec::new("/plugin/herdr-tether", vec!["resume".into()]),
                &PaneTitle::fallback(),
                Placement::SplitRight,
            )
            .unwrap_err();

        assert!(
            error.to_string().contains(expected),
            "unexpected placement error: {error:#}"
        );
    }
}

#[test]
fn replacement_inspects_source_then_closes_it_only_after_destination_is_running() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_replace_herdr(
        &binary,
        &log,
        r#"printf '%s' '{"id":"destination","result":{"type":"pane_process_info","process_info":{"pane_id":"w1:p9","shell_pid":303,"foreground_processes":[{"pid":404,"name":"tmux","argv":["/usr/bin/tmux","attach-session","-t","$7"],"cwd":"/tmp"}]}}}'"#,
        CLOSE_OK,
    );
    let client = HerdrClient::new(context(&binary));
    let title = PaneTitle::owned("local", "/srv/repository", None, Some("codex"));

    let inspection = client.inspect_replacement_source().unwrap();
    assert_eq!(inspection.pane_id, "w1:p1");
    assert!(inspection.requires_confirmation());
    assert!(inspection.safe_summary().contains("vim"));
    let pane = client
        .replace_current(
            &CommandSpec::new(
                "/usr/bin/tmux",
                vec!["attach-session".into(), "-t".into(), "$7".into()],
            ),
            &title,
        )
        .unwrap();
    assert_eq!(pane.pane_id, "w1:p9");

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("CALL\tpane\trename\tw1:p9\trepository · codex"));
    let source_info = transcript
        .find("CALL\tpane\tprocess-info\t--pane\tw1:p1")
        .unwrap();
    let split = transcript.find("CALL\tpane\tsplit").unwrap();
    let run = transcript.find("CALL\tpane\trun\tw1:p9").unwrap();
    let destination_info = transcript
        .find("CALL\tpane\tprocess-info\t--pane\tw1:p9")
        .unwrap();
    let close = transcript.find("CALL\tpane\tclose\tw1:p1").unwrap();
    assert!(
        source_info < split && split < run && run < destination_info && destination_info < close
    );
}

#[test]
fn replacement_close_failure_warns_without_invalidating_running_destination() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_replace_herdr(
        &binary,
        &log,
        r#"printf '%s' '{"id":"destination","result":{"type":"pane_process_info","process_info":{"pane_id":"w1:p9","foreground_processes":[{"pid":404,"name":"tmux","argv":["/usr/bin/tmux","attach-session","-t","$7"]}]}}}'"#,
        "exit 9",
    );

    let pane = HerdrClient::new(context(&binary))
        .replace_current(
            &CommandSpec::new(
                "/usr/bin/tmux",
                vec!["attach-session".into(), "-t".into(), "$7".into()],
            ),
            &PaneTitle::fallback(),
        )
        .unwrap();

    assert_eq!(pane.pane_id, "w1:p9");
    let warning = pane.warning.expect("source close failure is visible");
    assert!(warning.contains("destination `w1:p9` is running"));
    assert!(warning.contains("source pane `w1:p1` could not be closed"));
}

#[test]
fn replacement_preserves_source_when_destination_reports_unrelated_process() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_replace_herdr(
        &binary,
        &log,
        r#"printf '%s' '{"id":"destination","result":{"type":"pane_process_info","process_info":{"pane_id":"w1:p9","shell_pid":303,"foreground_processes":[{"pid":404,"name":"sh","argv":["sh","-c","exit 1"]}]}}}'"#,
        CLOSE_OK,
    );
    let error = HerdrClient::new(context(&binary))
        .replace_current(
            &CommandSpec::new(
                "/usr/bin/tmux",
                vec!["attach-session".into(), "-t".into(), "$7".into()],
            ),
            &PaneTitle::fallback(),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("source pane `w1:p1` was preserved")
    );
    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("CALL\tpane\tclose\tw1:p9"));
    assert!(!transcript.contains("CALL\tpane\tclose\tw1:p1"));
}

#[test]
fn replacement_preserves_reused_source_pane_id() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_reused_source_herdr(&binary, &log);

    let pane = HerdrClient::new(context(&binary))
        .replace_current(
            &CommandSpec::new(
                "/usr/bin/tmux",
                vec!["attach-session".into(), "-t".into(), "$7".into()],
            ),
            &PaneTitle::fallback(),
        )
        .unwrap();

    assert_eq!(pane.pane_id, "w1:p9");
    assert!(
        pane.warning
            .as_deref()
            .is_some_and(|warning| warning.contains("changed during replacement"))
    );
    let transcript = fs::read_to_string(log).unwrap();
    assert!(!transcript.contains("CALL\tpane\tclose\tw1:p1"));
}

#[test]
fn plugin_pane_open_defers_placement_to_the_manifest() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let client = HerdrClient::new(context(&binary));

    client.open_plugin_pane("picker").unwrap();

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains(
        "CALL\tplugin\tpane\topen\t--plugin\tmoneycaringcoder.tether\t--entrypoint\tpicker"
    ));
    // The manifest declares `placement = "popup"` with explicit sizing, so the
    // request must not override placement or size.
    assert!(!transcript.contains("--placement"));
    assert!(!transcript.contains("--width"));
    assert!(!transcript.contains("--height"));
}

#[test]
fn plugin_pane_open_does_not_probe_the_herdr_version() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");

    HerdrClient::new(context(&binary))
        .open_plugin_pane("picker")
        .unwrap();

    // Tether pins Herdr 0.8.0 through the manifest, so no capability sniffing
    // subprocess runs on the open path.
    assert!(!fs::read_to_string(log).unwrap().contains("CALL\t--version"));
}

#[test]
fn current_herdr_receives_source_owned_agent_view_group_token() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");
    let group_id = "build-group".parse::<OrchestrationGroupId>().unwrap();

    HerdrClient::new(context(&binary))
        .report_agent_view_group("w1:p9", &group_id, true)
        .unwrap();

    assert!(fs::read_to_string(log).unwrap().contains(
        "CALL\tpane\treport-metadata\tw1:p9\t--source\tplugin:moneycaringcoder.tether\t--token\ttether_group=build-group\t--token\ttether_remote=true"
    ));
}

#[test]
fn agent_view_group_token_reports_without_a_version_probe() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log, ":");

    HerdrClient::new(context(&binary))
        .report_agent_view_group(
            "w1:p9",
            &"build-group".parse::<OrchestrationGroupId>().unwrap(),
            false,
        )
        .unwrap();

    let transcript = fs::read_to_string(log).unwrap();
    // Herdr 0.8.0 always provides the metadata-token API, so the report goes out
    // directly instead of paying for a `herdr --version` subprocess first.
    assert!(!transcript.contains("CALL\t--version"));
    assert!(transcript.contains("report-metadata"));
}

#[test]
fn plugin_action_surfaces_real_stderr_error_envelope() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' 'herdr 0.8.0'; exit 0; fi\nprintf '%s' '{\"error\":{\"message\":\"invoking pane vanished\"}}' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();

    let error = HerdrClient::new(context(&binary))
        .open_plugin_pane("picker")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Herdr open plugin pane failed: invoking pane vanished"
    );
}

fn picker_fixture() -> (Config, State) {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let config = Config {
        version: Config::CURRENT_VERSION,
        notifications: Default::default(),
        hosts: vec![HostConfig {
            name: "build-box".into(),
            target: "builder@example.test".into(),
            roots: vec!["/srv/configured".into(), "/srv/shared".into()],
            presets: vec![CommandPreset {
                herdr_agent: None,
                name: "agent".into(),
                command: "exec codex".into(),
            }],
        }],
        discovery: DiscoveryDefaults {
            local_roots: vec!["~/code".into(), "/opt/work".into()],
            max_depth: 2,
            max_entries: 128,
            max_results: 12,
            timeout_seconds: 5,
            workers: 3,
        },
        retention: RetentionDefaults { closed_days: 14 },
        ui: UiDefaults {
            placement: Placement::SplitRight,
        },
    };
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: vec![
            SessionRecord {
                herdr_agent: None,
                id: "tether-0197f198000070008000000000000001"
                    .parse::<SessionId>()
                    .unwrap(),
                host: "build-box".into(),
                target: "builder@example.test".into(),
                directory: "/srv/shared".into(),
                preset: Some("agent".into()),
                command: Some("exec ${SHELL:-/bin/sh}".into()),
                tmux_session_id: None,
                ownership_proof: Some("0197f198000070008000000000000091".parse().unwrap()),
                status: SessionStatus::Running,
                created_at: now - Duration::days(2),
                last_used_at: now - Duration::hours(2),
                closed_at: None,
                exit_status: None,
            },
            SessionRecord {
                herdr_agent: None,
                id: "tether-0197f198000070008000000000000002"
                    .parse::<SessionId>()
                    .unwrap(),
                host: "build-box".into(),
                target: "builder@example.test".into(),
                directory: "/srv/recent".into(),
                preset: None,
                command: Some("exec ${SHELL:-/bin/sh}".into()),
                tmux_session_id: None,
                ownership_proof: Some("0197f198000070008000000000000092".parse().unwrap()),
                status: SessionStatus::Running,
                created_at: now - Duration::days(1),
                last_used_at: now - Duration::hours(1),
                closed_at: None,
                exit_status: None,
            },
        ],
        orchestration_groups: Vec::new(),
    };
    (config, state)
}

#[test]
fn plugin_invocation_location_uses_only_documented_absolute_cwds() {
    let global = r#"{"invocation_source":"command_palette","correlation_id":"global-1"}"#;
    let workspace =
        r#"{"workspace_id":"w1","workspace_cwd":"/home/user/code","invocation_source":"action"}"#;
    let pane = r#"{"workspace_cwd":"/home/user/code","focused_pane_id":"w1:p2","focused_pane_cwd":"/srv/shared"}"#;

    assert_eq!(
        InvocationLocation::from_plugin_context_json(Some(global)),
        None
    );
    assert_eq!(
        InvocationLocation::from_plugin_context_json(Some(workspace))
            .unwrap()
            .directory(),
        Path::new("/home/user/code")
    );
    assert_eq!(
        InvocationLocation::from_plugin_context_json(Some(pane))
            .unwrap()
            .directory(),
        Path::new("/srv/shared")
    );
}

#[test]
fn plugin_invocation_location_rejects_hostile_unknown_and_missing_context() {
    let oversized = format!(r#"{{"focused_pane_cwd":"/{}"}}"#, "a".repeat(4096));
    let cases = [
        None,
        Some(""),
        Some("{"),
        Some("[]"),
        Some(r#"{"focused_pane_cwd":17,"workspace_cwd":"/safe"}"#),
        Some(r#"{"focused_pane_cwd":"","workspace_cwd":"/safe"}"#),
        Some(r#"{"focused_pane_cwd":"relative","workspace_cwd":"/safe"}"#),
        Some("{\"focused_pane_cwd\":\"/safe\\u0000hostile\"}"),
        Some("{\"focused_pane_cwd\":\"/safe\\u001fhostile\"}"),
        Some(r#"{"workspace_cwd":false}"#),
        Some(r#"{"workspace_cwd":"relative"}"#),
        Some(r#"{"clicked_url":"file:///srv/shared","host":"build-box"}"#),
        Some(oversized.as_str()),
    ];

    for context in cases {
        assert_eq!(
            InvocationLocation::from_plugin_context_json(context),
            None,
            "unexpected preference for {context:?}"
        );
    }
}

#[test]
fn invocation_location_stably_prioritizes_only_authorized_picker_entries() {
    let (mut config, state) = picker_fixture();
    config.hosts.push(HostConfig {
        name: "other-box".into(),
        target: "other@example.test".into(),
        roots: vec!["/other/root".into()],
        presets: Vec::new(),
    });
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let before = options.clone();
    let preference = InvocationLocation::from_plugin_context_json(Some(
        r#"{"workspace_cwd":"/home/user/code","focused_pane_cwd":"/srv/shared"}"#,
    ));

    options.prefer_invocation_location(preference.as_ref(), &state);

    assert_eq!(
        options
            .hosts
            .iter()
            .map(|host| host.name.as_str())
            .collect::<Vec<_>>(),
        ["build-box", "local", "other-box"]
    );
    assert_eq!(
        options.hosts[0]
            .directories
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["/srv/shared", "/srv/recent", "/srv/configured"]
    );
    assert_eq!(
        options.hosts[0]
            .workloads
            .iter()
            .map(|workload| workload.id)
            .collect::<Vec<_>>(),
        [state.sessions[0].id, state.sessions[1].id]
    );
    assert_eq!(options.hosts[1], before.hosts[0]);
    assert_eq!(options.hosts[2], before.hosts[2]);

    let unchanged = options.clone();
    let unknown = InvocationLocation::from_plugin_context_json(Some(
        r#"{"focused_pane_cwd":"/untrusted/not-configured"}"#,
    ));
    options.prefer_invocation_location(unknown.as_ref(), &state);
    assert_eq!(options, unchanged);
    options.prefer_invocation_location(None, &state);
    assert_eq!(options, unchanged);
}

#[test]
fn worktree_siblings_are_preferred_after_the_invocation_directory() {
    let (config, state) = picker_fixture();
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let preference =
        InvocationLocation::from_plugin_context_json(Some(r#"{"focused_pane_cwd":"/srv/shared"}"#));

    // `/srv/configured` stands in for a sibling worktree of the same repository.
    options.prefer_invocation_location_with_worktrees(
        preference.as_ref(),
        &state,
        &[std::path::PathBuf::from("/srv/configured")],
    );

    // The invocation directory keeps first place; its sibling is pulled ahead of
    // everything else rather than displacing it.
    assert_eq!(
        options.hosts[0]
            .directories
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["/srv/shared", "/srv/configured", "/srv/recent"]
    );
}

#[test]
fn worktree_siblings_never_add_a_directory_the_picker_lacks() {
    let (config, state) = picker_fixture();
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let with_worktrees = {
        let mut options = options.clone();
        let preference = InvocationLocation::from_plugin_context_json(Some(
            r#"{"focused_pane_cwd":"/srv/shared"}"#,
        ));
        options.prefer_invocation_location_with_worktrees(
            preference.as_ref(),
            &state,
            // A real worktree the picker has no entry for, and one that is not a
            // directory of this host at all.
            &[
                std::path::PathBuf::from("/srv/worktrees/feature"),
                std::path::PathBuf::from("/untrusted/elsewhere"),
            ],
        );
        options
    };
    let preference =
        InvocationLocation::from_plugin_context_json(Some(r#"{"focused_pane_cwd":"/srv/shared"}"#));
    options.prefer_invocation_location(preference.as_ref(), &state);

    // Worktree paths are a preference over entries that already exist. An
    // unknown path must not appear, and must not change the ordering either.
    assert_eq!(with_worktrees, options);
}

/// Builds the two Git layouts that make worktree resolution hard, as real
/// directories, and returns the checkout paths and the Git directories that
/// belong to them.
///
/// `--separate-git-dir` puts the checkout's Git directory outside the checkout
/// and leaves a `.git` file behind. A submodule does the same thing with a
/// relative target inside the superproject's Git directory. In both cases the
/// checkout and its Git directory are different places, and only the checkout
/// is somewhere a user can work.
fn git_layouts(root: &Path) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let checkout = root.join("app");
    let sibling = root.join("app-feature");
    let separate = root.join("gitdirs/app");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::create_dir_all(separate.join("worktrees/app-feature")).unwrap();
    fs::write(separate.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", separate.display()),
    )
    .unwrap();
    fs::write(
        sibling.join(".git"),
        format!("gitdir: {}/worktrees/app-feature\n", separate.display()),
    )
    .unwrap();

    let superproject = root.join("super");
    let submodule = superproject.join("lib");
    let submodule_gitdir = superproject.join(".git/modules/lib");
    fs::create_dir_all(&submodule).unwrap();
    fs::create_dir_all(&submodule_gitdir).unwrap();
    fs::write(submodule.join(".git"), "gitdir: ../.git/modules/lib\n").unwrap();

    (
        vec![checkout, sibling, submodule],
        vec![separate.join("worktrees/app-feature"), submodule_gitdir],
    )
}

#[cfg(unix)]
fn serve_worktree_list(socket: &Path, reported: Vec<String>) -> std::thread::JoinHandle<()> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        std::io::BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "worktree.list");
        let id = request["id"].as_str().unwrap();
        let worktrees: Vec<serde_json::Value> = reported
            .iter()
            .map(|path| serde_json::json!({"path": path}))
            .collect();
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": id,
                "result": {"type": "worktree_list", "worktrees": worktrees},
            })
        )
        .unwrap();
    })
}

#[cfg(unix)]
#[test]
fn a_reported_git_directory_is_refused_rather_than_offered_as_a_worktree() {
    let temp = tempdir().unwrap();
    let (checkouts, git_directories) = git_layouts(temp.path());
    let socket = temp.path().join("herdr.sock");
    // A resolver that read a repository's Git directory — `git rev-parse
    // --git-dir`, or the `gitdir` file under `.git/worktrees/<name>` — instead
    // of the checkout would report exactly this: the checkouts, and the Git
    // directories of the linked worktree and the submodule.
    let reported = checkouts
        .iter()
        .chain(git_directories.iter())
        .map(|path| path.display().to_string())
        .collect();
    let server = serve_worktree_list(&socket, reported);

    let worktrees = HerdrSocketClient::new(socket)
        .worktree_paths(&checkouts[0])
        .unwrap();
    server.join().unwrap();

    // A submodule's Git directory is refused on its shape alone, because it
    // lives under the superproject's `.git`. The linked worktree's Git
    // directory is not: `--separate-git-dir` moved it outside the checkout, so
    // nothing about the path says what it is.
    assert_eq!(
        worktrees.paths,
        checkouts
            .iter()
            .cloned()
            .chain(std::iter::once(git_directories[0].clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(worktrees.rejected, 1);

    // The shape test cannot separate them, so the filesystem does: a checkout
    // holds a `.git` entry and a Git directory does not.
    for checkout in &checkouts {
        assert!(
            herdr_tether::discovery::is_checkout_directory(checkout),
            "{} is a checkout",
            checkout.display()
        );
    }
    for git_directory in &git_directories {
        assert!(
            !herdr_tether::discovery::is_checkout_directory(git_directory),
            "{} is a Git directory, not a checkout",
            git_directory.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn a_submodule_checkout_is_preferred_without_promoting_its_git_directory() {
    let temp = tempdir().unwrap();
    let (checkouts, git_directories) = git_layouts(temp.path());
    let (submodule, submodule_gitdir) = (&checkouts[2], &git_directories[1]);
    let (mut config, state) = picker_fixture();
    // The Git directory is configured as a root as well, so the ordering below
    // shows a refused path failing to promote an entry that does exist rather
    // than merely failing to invent one.
    config.hosts[0].roots = vec![
        submodule.display().to_string(),
        submodule_gitdir.display().to_string(),
        checkouts[0].display().to_string(),
    ];
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let context = format!(r#"{{"focused_pane_cwd":"{}"}}"#, checkouts[0].display());
    let preference = InvocationLocation::from_plugin_context_json(Some(&context));

    let socket = temp.path().join("herdr.sock");
    let reported = vec![
        submodule.display().to_string(),
        submodule_gitdir.display().to_string(),
    ];
    let server = serve_worktree_list(&socket, reported);
    let worktrees = HerdrSocketClient::new(socket)
        .worktree_paths(&checkouts[0])
        .unwrap();
    server.join().unwrap();

    options.prefer_invocation_location_with_worktrees(
        preference.as_ref(),
        &state,
        &worktrees.paths,
    );

    // The invoking checkout first, the submodule checkout pulled ahead of the
    // rest, and the submodule's Git directory left where it was, behind the
    // saved session directories it already sat behind.
    assert_eq!(
        options.hosts[0]
            .directories
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            checkouts[0].display().to_string(),
            submodule.display().to_string(),
            "/srv/recent".to_owned(),
            "/srv/shared".to_owned(),
            submodule_gitdir.display().to_string(),
        ]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
    );
}

#[test]
fn invocation_location_does_not_choose_between_ambiguous_hosts() {
    let (mut config, state) = picker_fixture();
    config.discovery.local_roots.push("/srv/shared".into());
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let before = options.clone();
    let preference =
        InvocationLocation::from_plugin_context_json(Some(r#"{"focused_pane_cwd":"/srv/shared"}"#));

    options.prefer_invocation_location(preference.as_ref(), &state);

    assert_eq!(options, before);
}

fn long_host_picker(count: usize) -> PickerState {
    let (config, state) = picker_fixture();
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let template = options.hosts[0].clone();
    options.hosts = (0..count)
        .map(|index| {
            let mut host = template.clone();
            host.name = format!("host-{index:02}");
            host.label =
                format!("host-{index:02}-with-a-label-that-is-deliberately-wider-than-the-panel");
            host.target = Some(format!("builder-{index:02}@example.test"));
            host
        })
        .collect();
    PickerState::new(options).unwrap()
}

#[test]
fn picker_status_refresh_surfaces_workload_limit_rejection_as_error_state() {
    let (config, state) = picker_fixture();
    let mut options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let workload = options.hosts[0].workloads[0].clone();
    options.hosts[0].workloads = vec![workload; MAX_STATUS_WORKLOADS + 1];
    let mut picker = PickerState::new(options).unwrap();
    let service = StatusService::new(
        ProcessBinaries::new("/bin/true", "/bin/true"),
        std::time::Duration::from_secs(1),
        1,
    );

    let error = picker.start_status_refresh(&service, 17).unwrap_err();

    assert_eq!(
        error,
        StatusRequestError::TooManyWorkloads {
            actual: MAX_STATUS_WORKLOADS + 1,
            maximum: MAX_STATUS_WORKLOADS,
        }
    );
    let rendered = render_picker_to_text(100, 20, &picker).unwrap();
    assert!(rendered.contains("error"));
    assert!(!error.to_string().contains("builder@example.test"));
}

#[test]
fn picker_uses_one_resize_fallback_until_the_minimum_geometry() {
    let picker = long_host_picker(20);
    for (width, height) in [(39, 8), (40, 7)] {
        let rendered = render_picker_to_text(width, height, &picker).unwrap();
        assert!(
            rendered.contains("Resize terminal to at least 40x8"),
            "{width}x{height}: {rendered:?}"
        );
        assert!(!rendered.contains("host-00"), "{width}x{height}");
        assert!(!rendered.contains("Enter choose"), "{width}x{height}");
    }

    let rendered = render_picker_to_text(40, 8, &picker).unwrap();
    assert!(!rendered.contains("Resize terminal"));
    assert!(rendered.contains("host-00"));
    assert!(rendered.contains('…'), "{rendered:?}");
    assert!(rendered.contains("Enter select"), "{rendered:?}");
    assert!(rendered.contains("Esc close"));
}

#[test]
fn picker_viewport_reports_position_and_both_directions_without_changing_selection() {
    let mut picker = long_host_picker(20);
    let cases = [
        (0, "1/20 · more below"),
        (9, "10/20 · more above · more below"),
        (19, "20/20 · more above"),
    ];
    let mut current = 0;
    for (target, metadata) in cases {
        for _ in current..target {
            picker.handle(PickerEvent::Next);
        }
        current = target;
        let rendered = render_picker_to_text(40, 8, &picker).unwrap();
        assert!(rendered.contains(metadata), "{metadata}: {rendered:?}");
        assert_eq!(rendered.lines().count(), 8);
        assert!(rendered.lines().all(|line| line.chars().count() <= 40));
    }
}

#[test]
fn picker_hides_removed_workloads_on_construction_and_lifecycle_refresh() {
    let (config, mut state) = picker_fixture();
    let removed_id = state.sessions[0].id;
    state.sessions[0].status = SessionStatus::Removed;
    state.sessions[0].closed_at = Some(state.sessions[0].last_used_at);
    state.sessions[0].directory = "/srv/removed-only".into();
    let ended_id = state.sessions[1].id;
    state.sessions[1].status = SessionStatus::Ended;
    state.sessions[1].closed_at = Some(state.sessions[1].last_used_at);
    let mut retained_removed = state.sessions[0].clone();
    retained_removed.id = "tether-0197f198000070008000000000000099".parse().unwrap();
    retained_removed.host = "removed-box".into();
    retained_removed.target = "removed@example.test".into();
    state.sessions.push(retained_removed);
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    assert!(
        options.hosts[0]
            .workloads
            .iter()
            .all(|workload| workload.id != removed_id)
    );
    assert!(
        !options.hosts[0]
            .directories
            .iter()
            .any(|path| path == "/srv/removed-only")
    );
    assert!(!options.hosts.iter().any(|host| host.name == "removed-box"));
    assert_eq!(state.sessions.len(), 3);
    assert_eq!(state.sessions[0].status, SessionStatus::Removed);

    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(7);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 7,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id: ended_id,
            generation: 7,
            action: PickerCloseAction::Remove,
        }
    );
    let mut removed_record = state.sessions[1].clone();
    removed_record.status = SessionStatus::Removed;
    assert!(picker.apply_close_result(PickerCloseResult {
        id: ended_id,
        generation: 7,
        record: Some(removed_record),
        error: None,
    }));
    assert!(picker.workload_label(ended_id).is_none());
}

#[test]
fn picker_orders_lifecycle_groups_recent_first_with_stable_ties() {
    let (mut config, mut state) = picker_fixture();
    config.hosts.push(HostConfig {
        name: "alpha-box".into(),
        target: "alpha@example.test".into(),
        roots: vec!["/srv/alpha".into()],
        presets: Vec::new(),
    });
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let template = state.sessions[0].clone();
    state.sessions.clear();
    let mut add = |suffix: &str,
                   host: &str,
                   target: &str,
                   directory: &str,
                   status: SessionStatus,
                   age_hours: i64| {
        let mut record = template.clone();
        record.id = format!("tether-0197f1980000700080000000000000{suffix}")
            .parse()
            .unwrap();
        record.host = host.into();
        record.target = target.into();
        record.directory = directory.into();
        record.status = status;
        record.last_used_at = now - Duration::hours(age_hours);
        record.closed_at = matches!(status, SessionStatus::Ended | SessionStatus::Removed)
            .then_some(record.last_used_at);
        state.sessions.push(record);
    };
    add(
        "12",
        "build-box",
        "builder@example.test",
        "/ended-new",
        SessionStatus::Ended,
        1,
    );
    add(
        "11",
        "build-box",
        "builder@example.test",
        "/running-old",
        SessionStatus::Running,
        8,
    );
    add(
        "10",
        "build-box",
        "builder@example.test",
        "/running-tie-a",
        SessionStatus::Running,
        2,
    );
    add(
        "09",
        "build-box",
        "builder@example.test",
        "/running-new",
        SessionStatus::Running,
        1,
    );
    add(
        "08",
        "build-box",
        "builder@example.test",
        "/running-tie-b",
        SessionStatus::Running,
        2,
    );
    add(
        "07",
        "build-box",
        "builder@example.test",
        "/stopping",
        SessionStatus::Stopping,
        1,
    );
    add(
        "06",
        "build-box",
        "builder@example.test",
        "/creating",
        SessionStatus::Creating,
        9,
    );
    add(
        "05",
        "alpha-box",
        "alpha@example.test",
        "/alpha-ended",
        SessionStatus::Ended,
        1,
    );
    add(
        "04",
        "alpha-box",
        "alpha@example.test",
        "/alpha-running",
        SessionStatus::Running,
        9,
    );

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    assert_eq!(
        options
            .hosts
            .iter()
            .map(|host| host.name.as_str())
            .collect::<Vec<_>>(),
        ["build-box", "alpha-box"]
    );
    let ordered = |host: &str| {
        options
            .hosts
            .iter()
            .find(|candidate| candidate.name == host)
            .unwrap()
            .workloads
            .iter()
            .map(|workload| (workload.status, workload.id.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ordered("build-box"),
        [
            (
                SessionStatus::Stopping,
                "tether-0197f198000070008000000000000007".into()
            ),
            (
                SessionStatus::Running,
                "tether-0197f198000070008000000000000009".into()
            ),
            (
                SessionStatus::Running,
                "tether-0197f198000070008000000000000008".into()
            ),
            (
                SessionStatus::Running,
                "tether-0197f198000070008000000000000010".into()
            ),
            (
                SessionStatus::Running,
                "tether-0197f198000070008000000000000011".into()
            ),
            (
                SessionStatus::Creating,
                "tether-0197f198000070008000000000000006".into()
            ),
            (
                SessionStatus::Ended,
                "tether-0197f198000070008000000000000012".into()
            ),
        ]
    );
    assert_eq!(
        ordered("alpha-box")
            .into_iter()
            .map(|(status, _)| status)
            .collect::<Vec<_>>(),
        [SessionStatus::Running, SessionStatus::Ended]
    );
}

#[test]
fn proofless_legacy_workload_offers_only_metadata_remove() {
    let (config, mut state) = picker_fixture();
    state.sessions.truncate(1);
    state.sessions[0].command = None;
    state.sessions[0].ownership_proof = None;
    let id = state.sessions[0].id;
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let workload = &options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap()
        .workloads[0];
    assert!(workload.label.contains("[legacy]"));
    assert!(workload.label.contains("Remove metadata"));
    assert!(!workload.label.contains("Open"));
    assert!(!workload.label.contains("Restart"));

    let mut picker = PickerState::new(options).unwrap();
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    assert!(picker.footer_text().contains("x Remove"));
    assert!(!picker.footer_text().contains("Enter Open"));
    assert!(!picker.footer_text().contains("Enter Restart"));
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);

    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id })
    );
    assert!(
        picker
            .footer_text()
            .contains("same-named tmux session is untouched")
    );
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id,
            generation: 0,
            action: PickerCloseAction::Remove,
        }
    );
}

#[test]
fn picker_retains_exact_removed_and_retargeted_lifecycle_groups() {
    let (mut config, mut state) = picker_fixture();
    config.hosts[0].target = "new-builder@example.test".into();
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    state.sessions.push(SessionRecord {
        herdr_agent: None,
        id: "tether-0197f198000070008000000000000003".parse().unwrap(),
        host: "removed-box".into(),
        target: "removed@example.test".into(),
        directory: "/srv/removed".into(),
        preset: None,
        command: Some("exec ${SHELL:-/bin/sh}".into()),
        tmux_session_id: None,
        ownership_proof: Some("0197f198000070008000000000000093".parse().unwrap()),
        status: SessionStatus::Running,
        created_at: now,
        last_used_at: now,
        closed_at: None,
        exit_status: None,
    });
    state.sessions[0].status = SessionStatus::Stopping;
    state.sessions[1].status = SessionStatus::Ended;
    state.sessions[1].closed_at = Some(now);

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    assert_eq!(options.hosts.len(), 3);
    assert_eq!(options.hosts[0].origin, PickerHostOrigin::Effective);
    assert_eq!(
        options.hosts[0].target.as_deref(),
        Some("new-builder@example.test")
    );
    assert!(options.hosts[0].allow_create);
    assert!(options.hosts[0].workloads.is_empty());
    assert_eq!(options.hosts[1].origin, PickerHostOrigin::Retained);
    assert_eq!(options.hosts[1].name, "build-box");
    assert_eq!(
        options.hosts[1].target.as_deref(),
        Some("builder@example.test")
    );
    assert!(!options.hosts[1].allow_create);
    assert_eq!(
        options.hosts[1]
            .workloads
            .iter()
            .map(|workload| workload.status)
            .collect::<Vec<_>>(),
        vec![SessionStatus::Stopping, SessionStatus::Ended]
    );
    assert!(
        options.hosts[1]
            .workloads
            .iter()
            .all(|workload| !workload.label.contains("Resume"))
    );
    let stopping = options.hosts[1]
        .workloads
        .iter()
        .find(|workload| workload.status == SessionStatus::Stopping)
        .unwrap();
    assert!(stopping.label.contains("[stopping]"));
    assert!(stopping.label.contains("Pending"));
    assert!(!stopping.label.to_lowercase().contains("retry"));
    assert_eq!(options.hosts[2].name, "removed-box");
    assert_eq!(options.hosts[2].workloads[0].status, SessionStatus::Running);

    let removed_id = options.hosts[2].workloads[0].id;
    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(4);
    assert!(picker.apply_status(StatusMessage::Workload {
        generation: 4,
        id: removed_id,
        status: WorkloadStatus::Running { attached: 0 },
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 4,
        host: "removed-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    picker.begin_discovery(4);
    assert!(!picker.apply_discovery(DiscoveryMessage::Repository {
        generation: 4,
        host: "removed-box".into(),
        path: "/must-not-append".into(),
    }));

    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Confirm);
    assert!(picker.footer_text().contains("Enter Open"));
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Placement);
    assert_eq!(
        picker.handle(PickerEvent::Confirm),
        PickerOutcome::Selected(PickerSelection::Resume {
            id: removed_id,
            placement: Placement::SplitRight,
        })
    );
}

#[test]
fn picker_walks_host_directory_command_and_placement() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let build_box = options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap();
    assert_eq!(
        build_box.directories,
        ["/srv/recent", "/srv/shared", "/srv/configured"]
    );
    assert_eq!(build_box.commands[0].label(), "Shell");
    assert_eq!(build_box.commands[1].label(), "agent");

    let mut picker = PickerState::new(options).unwrap();
    assert_eq!(picker.stage(), PickerStage::Host);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Directory);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Command);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Placement);
    picker.handle(PickerEvent::Next);
    let PickerOutcome::Selected(PickerSelection::Create(selection)) =
        picker.handle(PickerEvent::Confirm)
    else {
        panic!("picker did not return a create selection");
    };
    assert_eq!(selection.host, "build-box");
    assert_eq!(selection.directory, "/srv/recent");
    assert_eq!(selection.preset.as_deref(), Some("agent"));
    assert_eq!(selection.command, "exec codex");
    assert_eq!(selection.placement, Placement::SplitDown);
}

#[test]
fn picker_separates_recent_suggestions_from_configured_discovery_roots() {
    let (config, mut state) = picker_fixture();
    let mut local_recent = state.sessions[0].clone();
    local_recent.id = "tether-0197f198000070008000000000000004"
        .parse::<SessionId>()
        .unwrap();
    local_recent.host = "local".into();
    local_recent.target = "local".into();
    local_recent.directory = "/tmp/recent-local".into();
    state.sessions.push(local_recent);

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let local = options
        .hosts
        .iter()
        .find(|host| host.name == "local")
        .unwrap();
    assert_eq!(local.scan_roots, ["/home/user/code", "/opt/work"]);
    assert_eq!(
        local.directories,
        ["/tmp/recent-local", "/home/user/code", "/opt/work"]
    );

    let remote = options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap();
    assert_eq!(remote.scan_roots, ["/srv/configured", "/srv/shared"]);
    assert_eq!(
        remote.directories,
        ["/srv/recent", "/srv/shared", "/srv/configured"]
    );
    assert!(!remote.scan_roots.contains(&"/srv/recent".to_owned()));
}

#[test]
fn explorer_resumes_an_existing_workload_without_create_steps() {
    let (config, mut state) = picker_fixture();
    let mut closed = state.sessions[0].clone();
    closed.id = "tether-0197f198000070008000000000000003"
        .parse::<SessionId>()
        .unwrap();
    closed.status = SessionStatus::Ended;
    closed.closed_at = Some(closed.last_used_at);
    state.sessions.push(closed);

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let build_box = options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap();
    assert_eq!(build_box.workloads.len(), 3);
    assert!(
        build_box.workloads[0]
            .label
            .starts_with("[running] Tether · Open …00000002 · Shell · ")
    );
    assert!(build_box.workloads.iter().any(|workload| {
        workload.id.to_string() == "tether-0197f198000070008000000000000003"
            && workload.status == SessionStatus::Ended
    }));

    let expected_id = build_box.workloads[0].id;
    let mut explorer = PickerState::new(options).unwrap();
    explorer.begin_refresh(1);
    assert!(explorer.apply_status(StatusMessage::Host {
        generation: 1,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    assert!(explorer.apply_status(StatusMessage::Workload {
        generation: 1,
        id: expected_id,
        status: WorkloadStatus::Running { attached: 0 },
        checked_at: SystemTime::now(),
    }));
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Resource);
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Placement);

    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Selected(PickerSelection::Resume {
            id: expected_id,
            placement: Placement::SplitRight,
        })
    );
}

#[test]
fn explorer_uses_resource_stage_for_empty_catalog_create() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let mut explorer = PickerState::new(options).unwrap();

    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Resource);
    assert_eq!(
        explorer.resource_labels("local").unwrap(),
        ["Create new Tether workload"]
    );
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Directory);
    assert_eq!(explorer.handle(PickerEvent::Back), PickerOutcome::Continue);
    assert_eq!(explorer.stage(), PickerStage::Resource);
    assert_eq!(explorer.handle(PickerEvent::Back), PickerOutcome::Continue);
    assert_eq!(explorer.stage(), PickerStage::Host);
}

#[test]
fn explorer_orders_owned_external_create_and_returns_exact_external_intent() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut explorer = PickerState::new(options).unwrap();
    explorer.begin_refresh(1);
    assert!(explorer.apply_status(StatusMessage::Catalog {
        generation: 1,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: vec![
            ExternalSession {
                name: "alpha".parse().unwrap(),
                attached: 0,
            },
            ExternalSession {
                name: "work box".parse().unwrap(),
                attached: 2,
            },
        ],
        hidden_reserved: 1,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));

    let labels = explorer.resource_labels("build-box").unwrap();
    assert!(labels[0].contains("Open …00000002"));
    assert!(labels[1].contains("Open …00000001"));
    assert_eq!(labels[2], "[external · running] alpha");
    assert_eq!(labels[3], "[external · running · 2 attached] work box");
    assert_eq!(labels[4], "Create new Tether workload");

    explorer.handle(PickerEvent::Confirm);
    explorer.handle(PickerEvent::Next);
    explorer.handle(PickerEvent::Next);
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Placement);
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Selected(PickerSelection::AttachExternal {
            host: "build-box".into(),
            target: Some("builder@example.test".into()),
            name: "alpha".parse::<ExternalSessionName>().unwrap(),
            placement: Placement::SplitRight,
        })
    );
}

#[test]
fn external_selection_survives_rebuild_and_failed_refresh_stays_stale() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut explorer = PickerState::new(options).unwrap();
    explorer.begin_refresh(1);
    assert!(explorer.apply_status(StatusMessage::Catalog {
        generation: 1,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: vec![ExternalSession {
            name: "zeta".parse().unwrap(),
            attached: 0,
        }],
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    explorer.handle(PickerEvent::Confirm);
    explorer.handle(PickerEvent::Next);
    explorer.handle(PickerEvent::Next);

    assert!(explorer.apply_status(StatusMessage::Catalog {
        generation: 1,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: vec![
            ExternalSession {
                name: "alpha".parse().unwrap(),
                attached: 0,
            },
            ExternalSession {
                name: "zeta".parse().unwrap(),
                attached: 0,
            },
        ],
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Placement);
    explorer.begin_refresh(2);
    assert!(explorer.apply_status(StatusMessage::Catalog {
        generation: 2,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: vec![
            ExternalSession {
                name: "aardvark".parse().unwrap(),
                attached: 0,
            },
            ExternalSession {
                name: "alpha".parse().unwrap(),
                attached: 0,
            },
            ExternalSession {
                name: "zeta".parse().unwrap(),
                attached: 0,
            },
        ],
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    explorer.handle(PickerEvent::Back);

    explorer.begin_refresh(3);
    assert!(explorer.resource_labels("build-box").unwrap()[4].starts_with("[stale] [external"));
    assert!(!explorer.apply_status(StatusMessage::Catalog {
        generation: 2,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: Vec::new(),
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    assert!(explorer.apply_status(StatusMessage::Catalog {
        generation: 3,
        host: "build-box".into(),
        status: ExternalCatalogStatus::TimedOut,
        sessions: Vec::new(),
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    assert!(explorer.resource_labels("build-box").unwrap()[4].starts_with("[stale] [external"));
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Selected(PickerSelection::AttachExternal {
            host: "build-box".into(),
            target: Some("builder@example.test".into()),
            name: "zeta".parse().unwrap(),
            placement: Placement::SplitRight,
        })
    );
}

#[test]
fn status_updates_progressively_and_refresh_rejects_stale_generation() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let workload_id = options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap()
        .workloads[0]
        .id;
    let mut explorer = PickerState::new(options).unwrap();
    explorer.handle(PickerEvent::Next);
    explorer.handle(PickerEvent::Confirm);
    assert_eq!(explorer.stage(), PickerStage::Resource);

    explorer.begin_refresh(1);
    assert_eq!(
        explorer.host_label("build-box"),
        Some("[loading] build-box")
    );
    assert_eq!(explorer.stage(), PickerStage::Resource);

    assert!(explorer.apply_status(StatusMessage::Host {
        generation: 1,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert_eq!(explorer.host_label("build-box"), Some("[online] build-box"));
    assert_eq!(explorer.host_label("local"), Some("[loading] local"));
    assert!(explorer.apply_status(StatusMessage::Workload {
        generation: 1,
        id: workload_id,
        status: WorkloadStatus::Running { attached: 2 },
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert!(
        explorer
            .workload_label(workload_id)
            .unwrap()
            .starts_with("[running · 2 attached] [running] Tether · Open …00000002")
    );

    explorer.begin_refresh(2);
    assert_eq!(
        explorer.host_label("build-box"),
        Some("[stale: online] build-box")
    );
    assert!(!explorer.apply_status(StatusMessage::Host {
        generation: 1,
        host: "build-box".into(),
        status: HostReachability::Unreachable,
        detail: None,
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert_eq!(
        explorer.host_label("build-box"),
        Some("[stale: online] build-box")
    );
    assert!(explorer.apply_status(StatusMessage::Host {
        generation: 2,
        host: "build-box".into(),
        status: HostReachability::TimedOut,
        detail: Some("tmux missing; install it".into()),
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert_eq!(
        explorer.host_label("build-box"),
        Some("[timeout] build-box · tmux missing; install it")
    );
}

#[test]
fn fresh_missing_workload_cannot_be_resumed() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let workload_id = options.hosts[0].workloads[0].id;
    let mut explorer = PickerState::new(options).unwrap();
    explorer.begin_refresh(1);
    assert!(explorer.apply_status(StatusMessage::Workload {
        generation: 1,
        id: workload_id,
        status: WorkloadStatus::Missing,
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    explorer.handle(PickerEvent::Confirm);
    assert_eq!(explorer.stage(), PickerStage::Resource);

    assert_eq!(
        explorer.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Resource);
}

#[test]
fn refresh_event_requests_work_without_resetting_navigation() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let mut explorer = PickerState::new(options).unwrap();
    explorer.handle(PickerEvent::Confirm);
    explorer.handle(PickerEvent::Confirm);
    assert_eq!(explorer.stage(), PickerStage::Directory);

    assert_eq!(
        explorer.handle(PickerEvent::Refresh),
        PickerOutcome::RefreshRequested
    );
    assert_eq!(explorer.stage(), PickerStage::Directory);
}

#[test]
fn discovery_appends_after_seed_directories_and_ignores_old_generations() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut explorer = PickerState::new(options).unwrap();
    explorer.begin_discovery(4);

    assert!(explorer.apply_discovery(DiscoveryMessage::Repository {
        generation: 4,
        host: "build-box".into(),
        path: "/srv/discovered".into(),
    }));
    assert_eq!(
        explorer.directory_paths("build-box").unwrap(),
        [
            "/srv/recent",
            "/srv/shared",
            "/srv/configured",
            "/srv/discovered"
        ]
    );
    assert!(!explorer.apply_discovery(DiscoveryMessage::Repository {
        generation: 3,
        host: "build-box".into(),
        path: "/srv/stale".into(),
    }));
    assert!(explorer.apply_discovery(DiscoveryMessage::HostFinished {
        generation: 4,
        host: "build-box".into(),
        completion: DiscoveryCompletion::Complete,
    }));
    assert!(
        !explorer
            .directory_paths("build-box")
            .unwrap()
            .contains(&"/srv/stale")
    );
}

#[test]
fn directory_filter_and_direct_path_preserve_create_flow() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut explorer = PickerState::new(options).unwrap();
    explorer.handle(PickerEvent::Confirm);
    explorer.handle(PickerEvent::Next);
    explorer.handle(PickerEvent::Next);
    explorer.handle(PickerEvent::Confirm);
    assert_eq!(explorer.stage(), PickerStage::Directory);

    explorer.handle(PickerEvent::BeginFilter);
    for character in "shared".chars() {
        explorer.handle(PickerEvent::Insert(character));
    }
    assert_eq!(explorer.input(), &PickerInput::Filter("shared".into()));
    assert_eq!(explorer.visible_directories(), vec!["/srv/shared"]);
    explorer.handle(PickerEvent::ExitInput);

    explorer.handle(PickerEvent::BeginPath);
    for character in "/tmp/direct path".chars() {
        explorer.handle(PickerEvent::Insert(character));
    }
    assert_eq!(
        explorer.handle(PickerEvent::SubmitInput),
        PickerOutcome::Continue
    );
    assert_eq!(explorer.stage(), PickerStage::Command);
    explorer.handle(PickerEvent::Confirm);
    let PickerOutcome::Selected(PickerSelection::Create(selection)) =
        explorer.handle(PickerEvent::Confirm)
    else {
        panic!("direct path did not produce a create selection");
    };
    assert_eq!(selection.directory, "/tmp/direct path");
}

#[test]
fn cancelling_picker_produces_no_selection() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", true);
    let mut picker = PickerState::new(options).unwrap();
    assert_eq!(picker.handle(PickerEvent::Cancel), PickerOutcome::Cancelled);
}

fn owned_close_picker() -> (PickerState, SessionId, SessionId) {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let ids = options.hosts[0]
        .workloads
        .iter()
        .map(|workload| workload.id)
        .collect::<Vec<_>>();
    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(7);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 7,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    for id in &ids {
        assert!(picker.apply_status(StatusMessage::Workload {
            generation: 7,
            id: *id,
            status: WorkloadStatus::Running { attached: 0 },
            checked_at: SystemTime::now(),
        }));
    }
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    (picker, ids[0], ids[1])
}

#[test]
fn owned_close_requires_explicit_confirmation_and_pending_close_cannot_be_abandoned() {
    let (mut picker, selected_id, _) = owned_close_picker();

    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: selected_id })
    );
    assert_eq!(picker.frame_title(), "Confirm Stop");
    assert!(picker.footer_text().starts_with("y confirm · n/Esc keep"));
    assert!(picker.footer_text().contains(&selected_id.to_string()));
    assert_eq!(
        picker.handle(PickerEvent::DismissClose),
        PickerOutcome::Continue
    );
    assert!(picker.close_modal().is_none());
    assert!(!picker.close_busy());

    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id: selected_id,
            generation: 7,
            action: PickerCloseAction::Stop,
        }
    );
    assert!(picker.close_busy());
    assert_eq!(picker.frame_title(), "Applying lifecycle action");
    assert!(picker.footer_text().contains("wait"));
    assert_eq!(picker.handle(PickerEvent::Cancel), PickerOutcome::Continue);
    assert_eq!(picker.handle(PickerEvent::Back), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    assert_eq!(
        picker.handle(PickerEvent::DismissClose),
        PickerOutcome::Continue
    );
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::Continue
    );
    assert_eq!(picker.handle(PickerEvent::Next), PickerOutcome::Continue);
}

#[test]
fn contextual_actions_survive_refresh_and_never_attach_ended_or_unreachable_workloads() {
    let (config, mut state) = picker_fixture();
    state.sessions[0].status = SessionStatus::Ended;
    state.sessions[0].closed_at = Some(state.sessions[0].last_used_at);
    let ended_id = state.sessions[0].id;
    let running_id = state.sessions[1].id;
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(7);
    picker.handle(PickerEvent::Confirm);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 7,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    picker.handle(PickerEvent::Next);

    assert!(picker.footer_text().contains("Enter Restart"));
    assert!(picker.footer_text().contains("x Remove"));
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(
        picker.handle(PickerEvent::Confirm),
        PickerOutcome::Selected(PickerSelection::Restart {
            id: ended_id,
            placement: Placement::SplitRight,
        })
    );

    picker.handle(PickerEvent::Back);
    picker.handle(PickerEvent::Previous);
    assert!(picker.apply_status(StatusMessage::Workload {
        generation: 7,
        id: running_id,
        status: WorkloadStatus::Missing,
        checked_at: SystemTime::now(),
    }));
    assert!(picker.footer_text().contains("Enter Restart"));
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: running_id })
    );
    picker.begin_refresh(8);
    assert!(picker.close_modal().is_some());
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id: running_id,
            generation: 8,
            action: PickerCloseAction::Remove,
        }
    );

    let mut unreachable = PickerState::new(PickerOptions::from_config_state(
        &config,
        &picker_fixture().1,
        "/home/user",
        false,
    ))
    .unwrap();
    unreachable.begin_refresh(3);
    unreachable.handle(PickerEvent::Confirm);
    assert!(unreachable.apply_status(StatusMessage::Host {
        generation: 3,
        host: "build-box".into(),
        status: HostReachability::Unreachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    assert_eq!(
        unreachable.handle(PickerEvent::Confirm),
        PickerOutcome::Continue
    );
    assert_eq!(
        unreachable.handle(PickerEvent::Close),
        PickerOutcome::Continue
    );
    assert!(unreachable.close_modal().is_none());
    assert!(unreachable.footer_text().contains("r Retry"));
}

#[test]
fn close_is_owned_only_and_cached_status_never_skips_confirmation() {
    let (mut picker, owned_id, _) = owned_close_picker();
    assert!(picker.apply_status(StatusMessage::Workload {
        generation: 7,
        id: owned_id,
        status: WorkloadStatus::Missing,
        checked_at: SystemTime::now(),
    }));
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: owned_id })
    );
    assert_eq!(picker.frame_title(), "Confirm Remove");
    picker.handle(PickerEvent::DismissClose);

    picker.begin_refresh(8);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert!(picker.close_modal().is_none());
    assert!(picker.footer_text().contains("r Refresh"));

    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert!(picker.close_modal().is_none());
}

#[test]
fn close_success_retains_exact_row_as_authoritative_closed_metadata() {
    let (_, state) = picker_fixture();
    let (mut picker, first_id, second_id) = owned_close_picker();
    let mut record = state
        .sessions
        .into_iter()
        .find(|record| record.id == first_id)
        .unwrap();
    record.status = SessionStatus::Ended;
    record.closed_at = Some(record.last_used_at);
    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id: first_id,
            generation: 7,
            action: PickerCloseAction::Stop,
        }
    );
    assert!(picker.apply_close_result(PickerCloseResult {
        id: first_id,
        generation: 7,
        error: None,
        record: Some(record),
    }));

    let labels = picker.resource_labels("build-box").unwrap();
    assert_eq!(labels.len(), 3);
    assert!(labels.iter().any(|label| label.contains("ended")));
    assert!(labels[0].contains("00000001"));
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: first_id })
    );
    picker.handle(PickerEvent::DismissClose);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: second_id })
    );
    assert!(!picker.apply_close_result(PickerCloseResult {
        id: first_id,
        generation: 7,
        error: None,
        record: None,
    }));
}

#[test]
fn a_workload_that_ended_with_a_failing_status_reads_apart_from_a_clean_end() {
    let (config, mut state) = picker_fixture();
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    for session in &mut state.sessions {
        session.status = SessionStatus::Ended;
        session.closed_at = Some(now);
    }
    state.sessions[0].exit_status = Some(1);
    state.sessions[1].exit_status = Some(0);
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);

    let labels: Vec<&str> = options.hosts[0]
        .workloads
        .iter()
        .map(|workload| workload.label.as_str())
        .collect();
    // The failing one is named, the clean one keeps the ordinary word, and both
    // still offer an explicit Restart rather than anything automatic.
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("[failed] Tether · Restart")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("[ended] Tether · Restart")),
        "{labels:?}"
    );

    // An end whose status tmux could not report is not a failure.
    state.sessions[0].exit_status = None;
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    assert!(
        options.hosts[0]
            .workloads
            .iter()
            .all(|workload| !workload.label.contains("[failed]")),
        "an unknown outcome must not read as a failure"
    );
}

#[test]
fn a_workload_that_failed_immediately_paces_its_restart_and_says_why() {
    let (config, mut state) = picker_fixture();
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    // The first workload's command died 400ms after it started; the second ran
    // for a quarter of an hour before failing.
    state.sessions[0].status = SessionStatus::Ended;
    state.sessions[0].last_used_at = now - Duration::seconds(30);
    state.sessions[0].closed_at = Some(now - Duration::seconds(30) + Duration::milliseconds(400));
    state.sessions[0].exit_status = Some(1);
    state.sessions[1].status = SessionStatus::Ended;
    state.sessions[1].last_used_at = now - Duration::minutes(20);
    state.sessions[1].closed_at = Some(now - Duration::minutes(5));
    state.sessions[1].exit_status = Some(1);

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let labels: Vec<&str> = options.hosts[0]
        .workloads
        .iter()
        .map(|workload| workload.label.as_str())
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("[failed immediately] Tether · Restart")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("[failed] Tether · Restart")),
        "a failure that took its time is not a loop: {labels:?}"
    );

    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(7);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 7,
        host: "build-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    picker.handle(PickerEvent::Confirm);

    // While the pace lasts, Restart is not offered and the footer explains it.
    let paced = picker.footer_text_at(now);
    assert!(paced.contains("Failed immediately"), "{paced}");
    assert!(paced.contains("Restart paced"), "{paced}");
    assert!(paced.contains("never restarts on its own"), "{paced}");
    assert!(!paced.contains("Enter Restart"), "{paced}");
    // Metadata removal stays available, so the row is not a dead end.
    assert!(paced.contains("x Remove"), "{paced}");

    // Once it has elapsed the action returns, unchanged.
    let after = picker.footer_text_at(now + Duration::minutes(1));
    assert!(after.contains("Enter Restart"), "{after}");
    assert!(!after.contains("Restart paced"), "{after}");
}

#[test]
fn successful_close_with_authoritative_absence_removes_exact_retained_group() {
    let (config, mut state) = picker_fixture();
    let record = state.sessions.remove(0);
    let id = record.id;
    state.sessions = vec![SessionRecord {
        host: "removed-box".into(),
        target: "removed@example.test".into(),
        ..record
    }];
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(7);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 7,
        host: "removed-box".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    assert!(picker.apply_status(StatusMessage::Workload {
        generation: 7,
        id,
        status: WorkloadStatus::Running { attached: 0 },
        checked_at: SystemTime::now(),
    }));
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Confirm);
    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id,
            generation: 7,
            action: PickerCloseAction::Stop
        }
    );

    assert!(picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: None,
        record: None,
    }));
    assert!(picker.workload_label(id).is_none());
    assert!(picker.host_label("removed-box").is_none());
}

#[test]
fn unreadable_authoritative_reread_blocks_active_resume_but_allows_close_retry() {
    let (mut picker, id, _) = owned_close_picker();
    picker.handle(PickerEvent::Close);
    picker.handle(PickerEvent::ConfirmClose);
    assert!(picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: Some("authoritative state reread failed".into()),
        record: None,
    }));
    picker.handle(PickerEvent::DismissClose);

    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);
    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id })
    );
}

#[test]
fn close_failure_is_sanitized_persistence_neutral_non_resumable_and_retryable() {
    let (mut picker, id, _) = owned_close_picker();
    let (_, state) = picker_fixture();
    let mut record = state
        .sessions
        .into_iter()
        .find(|record| record.id == id)
        .unwrap();
    record.status = SessionStatus::Stopping;
    picker.handle(PickerEvent::Close);
    picker.handle(PickerEvent::ConfirmClose);
    assert!(picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: Some("backend\u{1b}[31m failed\nretry".into()),
        record: Some(record),
    }));
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Failed {
            id,
            error: "backend failed retry".into(),
        })
    );
    assert!(
        picker
            .resource_labels("build-box")
            .unwrap()
            .iter()
            .any(|label| label.starts_with("[action failed · x retry]"))
    );
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id,
            generation: 7,
            action: PickerCloseAction::Stop
        }
    );
}

#[test]
fn close_error_formatter_includes_source_chain_and_sanitizes_terminal_text() {
    let (_, id, _) = owned_close_picker();
    let text = format_close_error(CloseOwnedError::Inspect {
        id,
        source: anyhow::anyhow!("helper\u{1b}[31m source\u{1b}]0;spoof\u{7}\ncause"),
    });

    assert!(text.starts_with(&format!("inspect session `{id}`")));
    assert!(text.contains("helper source cause"));
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\n'));
    assert!(!text.contains("spoof"));
}

#[test]
fn refresh_accepts_confirmed_close_result_from_its_original_generation() {
    let (mut picker, id, _) = owned_close_picker();
    picker.handle(PickerEvent::Close);
    picker.handle(PickerEvent::ConfirmClose);
    picker.begin_refresh(8);
    assert!(picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: None,
        record: None,
    }));
    assert_eq!(picker.resource_labels("build-box").unwrap().len(), 2);
}

#[test]
fn external_and_create_resources_cannot_represent_close_requests() {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::new(options).unwrap();
    picker.begin_refresh(3);
    assert!(picker.apply_status(StatusMessage::Catalog {
        generation: 3,
        host: "build-box".into(),
        status: ExternalCatalogStatus::Available,
        sessions: vec![ExternalSession {
            name: "external-only".parse::<ExternalSessionName>().unwrap(),
            attached: 0,
        }],
        hidden_reserved: 0,
        hidden_unsafe: 0,
        checked_at: SystemTime::now(),
    }));
    picker.handle(PickerEvent::Confirm);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);

    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert!(picker.close_modal().is_none());
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert!(picker.close_modal().is_none());
    assert!(!picker.close_busy());
}

fn prune_preview(days: u64, count: usize) -> PrunePreview {
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let now = Utc::now();
    let sessions = (0..count)
        .map(|index| SessionRecord {
            herdr_agent: None,
            id: format!("tether-0197f198000070008000000000000{:03}", index + 100)
                .parse()
                .unwrap(),
            host: "archived".into(),
            target: "nobody@example.test".into(),
            directory: "/closed".into(),
            preset: None,
            command: Some("exec ${SHELL:-/bin/sh}".into()),
            tmux_session_id: None,
            ownership_proof: None,
            status: SessionStatus::Ended,
            created_at: now - Duration::days(60),
            last_used_at: now - Duration::days(40),
            closed_at: Some(now - Duration::days(40)),
            exit_status: None,
        })
        .collect();
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions,
            orchestration_groups: Vec::new(),
        })
        .unwrap();
    PruneService::new(store).preview(days).unwrap()
}

#[test]
fn prune_reconciliation_removes_only_returned_ids_and_empty_retained_groups() {
    let (config, _) = picker_fixture();
    let now = Utc::now();
    let records = (0..3)
        .map(|index| SessionRecord {
            herdr_agent: None,
            id: format!("tether-0197f198000070008000000000000{:03}", index + 100)
                .parse()
                .unwrap(),
            host: if index == 2 { "lone" } else { "archived" }.into(),
            target: "removed@example.test".into(),
            directory: "/closed".into(),
            preset: None,
            command: Some("exec ${SHELL:-/bin/sh}".into()),
            tmux_session_id: None,
            ownership_proof: None,
            status: SessionStatus::Ended,
            created_at: now - Duration::days(60),
            last_used_at: now - Duration::days(40),
            closed_at: Some(now - Duration::days(40)),
            exit_status: None,
        })
        .collect::<Vec<_>>();
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: records.clone(),
        orchestration_groups: Vec::new(),
    };
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    store.save(&state).unwrap();
    let preview = PruneService::new(store).preview(14).unwrap();
    let removed = vec![records[0].id, records[2].id];
    let skipped = vec![records[1].id];
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::with_retention(options, 14).unwrap();
    picker.begin_refresh(11);
    picker.handle(PickerEvent::BeginPrune);
    picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    });
    picker.handle(PickerEvent::ConfirmPrune);

    assert!(picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview,
        removed_ids: Some(removed),
        skipped_ids: Some(skipped),
        error: None,
    }));
    assert!(picker.workload_label(records[0].id).is_none());
    assert!(picker.workload_label(records[1].id).is_some());
    assert!(picker.workload_label(records[2].id).is_none());
    assert!(picker.host_label("lone").is_none());
    assert!(picker.host_label("archived").is_some());
}

#[test]
fn prune_preserves_selected_exact_resource_when_an_earlier_row_is_removed() {
    let (config, _) = picker_fixture();
    let now = Utc::now();
    let mut records = Vec::new();
    for (suffix, status, age) in [
        (200, SessionStatus::Ended, 40),
        (201, SessionStatus::Ended, 41),
        (202, SessionStatus::Stopping, 42),
    ] {
        records.push(SessionRecord {
            herdr_agent: None,
            id: format!("tether-0197f198000070008000000000000{suffix:03}")
                .parse()
                .unwrap(),
            host: "archived".into(),
            target: "removed@example.test".into(),
            directory: "/closed".into(),
            preset: None,
            command: Some("exec ${SHELL:-/bin/sh}".into()),
            tmux_session_id: None,
            ownership_proof: Some("0197f198000070008000000000000094".parse().unwrap()),
            status,
            created_at: now - Duration::days(60),
            last_used_at: now - Duration::days(age),
            closed_at: (status == SessionStatus::Ended).then_some(now - Duration::days(age)),
            exit_status: None,
        });
    }
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: records.clone(),
        orchestration_groups: Vec::new(),
    };
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    store.save(&state).unwrap();
    let preview = PruneService::new(store).preview(14).unwrap();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::with_retention(options, 14).unwrap();
    picker.begin_refresh(11);
    assert!(picker.apply_status(StatusMessage::Host {
        generation: 11,
        host: "archived".into(),
        status: HostReachability::Reachable,
        detail: None,
        checked_at: SystemTime::now(),
    }));
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Confirm);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::BeginPrune);
    picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    });
    picker.handle(PickerEvent::ConfirmPrune);
    picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview,
        removed_ids: Some(vec![records[0].id]),
        skipped_ids: Some(vec![records[1].id]),
        error: None,
    });

    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: records[1].id })
    );
}

#[test]
fn prune_of_final_retained_host_resets_empty_picker_to_safe_host_stage() {
    let (mut config, mut state) = picker_fixture();
    config.hosts.clear();
    state.sessions.truncate(1);
    state.sessions[0].host = "removed".into();
    state.sessions[0].target = "removed@example.test".into();
    state.sessions[0].status = SessionStatus::Ended;
    state.sessions[0].closed_at = Some(state.sessions[0].last_used_at);
    state.sessions[0].last_used_at = Utc::now() - Duration::days(40);
    state.sessions[0].created_at = Utc::now() - Duration::days(60);
    state.sessions[0].closed_at = Some(state.sessions[0].last_used_at);
    let id = state.sessions[0].id;
    let temp = tempdir().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    store.save(&state).unwrap();
    let preview = PruneService::new(store).preview(14).unwrap();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::with_retention(options, 14).unwrap();
    picker.begin_refresh(11);
    picker.handle(PickerEvent::Confirm);
    picker.handle(PickerEvent::BeginPrune);
    picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    });
    picker.handle(PickerEvent::ConfirmPrune);

    assert!(picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview,
        removed_ids: Some(vec![id]),
        skipped_ids: Some(Vec::new()),
        error: None,
    }));
    assert_eq!(picker.stage(), PickerStage::Host);
    assert!(picker.footer_text().contains("navigate"));
    assert!(!picker.footer_text().contains("Enter select"));
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Host);
    assert!(!picker.footer_text().contains("Enter select"));
}

fn prune_picker() -> PickerState {
    let (config, state) = picker_fixture();
    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let mut picker = PickerState::with_retention(options, config.retention.closed_days).unwrap();
    picker.begin_refresh(11);
    picker
}

#[test]
fn global_prune_preview_is_selection_independent_and_requires_explicit_confirmation() {
    let mut picker = prune_picker();
    let before = picker.footer_text();
    assert!(!before.contains("prune"));
    assert_eq!(
        picker.handle(PickerEvent::BeginPrune),
        PickerOutcome::PrunePreviewRequested {
            older_than_days: 14,
            generation: 11,
        }
    );
    assert!(picker.prune_busy());
    assert_eq!(
        picker.handle(PickerEvent::Cancel),
        PickerOutcome::Cancelled,
        "read-only preview may be abandoned before confirmation"
    );
    for event in [
        PickerEvent::Back,
        PickerEvent::Confirm,
        PickerEvent::Refresh,
        PickerEvent::Close,
        PickerEvent::BeginPrune,
    ] {
        assert_eq!(picker.handle(event), PickerOutcome::Continue);
    }

    let preview = prune_preview(14, 2);
    assert!(picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    }));
    assert_eq!(
        picker.prune_modal(),
        Some(&PickerPruneModal::Confirm {
            preview: preview.clone()
        })
    );
    let footer = picker.footer_text();
    assert!(footer.contains("2 closed metadata"));
    assert!(footer.contains("14 days"));
    assert!(footer.contains("No host contact"));
    assert!(footer.contains("y confirm"));
    assert!(footer.contains("n/Esc keep"));
    assert_eq!(
        picker.handle(PickerEvent::DismissPrune),
        PickerOutcome::Continue
    );
    assert!(picker.prune_modal().is_none());

    assert_eq!(
        picker.handle(PickerEvent::BeginPrune),
        PickerOutcome::PrunePreviewRequested {
            older_than_days: 14,
            generation: 11,
        }
    );
    assert!(picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    }));
    assert_eq!(
        picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::PruneApplyRequested {
            preview,
            generation: 11,
        }
    );
    assert!(picker.prune_busy());
    for event in [
        PickerEvent::Cancel,
        PickerEvent::Back,
        PickerEvent::Confirm,
        PickerEvent::Refresh,
        PickerEvent::Close,
        PickerEvent::BeginPrune,
    ] {
        assert_eq!(
            picker.handle(event),
            PickerOutcome::Continue,
            "confirmed prune cannot be abandoned"
        );
    }
}

#[test]
fn prune_zero_success_mismatch_and_failures_are_truthful_bounded_and_retryable() {
    let mut picker = prune_picker();
    let initial_labels = picker.resource_labels("build-box").unwrap();
    picker.handle(PickerEvent::BeginPrune);
    assert!(!picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 10,
        result: Ok(prune_preview(14, 1)),
    }));
    assert!(picker.prune_busy());
    assert!(picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Err("preview\u{1b}[31m failed\n".repeat(100)),
    }));
    assert_eq!(picker.prune_phase(), Some(PickerPrunePhase::Preview));
    assert!(!picker.footer_text().contains('\u{1b}'));
    assert!(picker.footer_text().chars().count() < 400);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::PrunePreviewRequested {
            older_than_days: 14,
            generation: 11,
        }
    );

    let empty = prune_preview(14, 0);
    assert!(picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(empty),
    }));
    assert!(picker.prune_modal().is_none());
    assert!(picker.footer_text().contains("No closed metadata eligible"));

    picker.handle(PickerEvent::BeginPrune);
    let preview = prune_preview(14, 2);
    picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    });
    picker.handle(PickerEvent::ConfirmPrune);
    assert!(picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview: preview.clone(),
        removed_ids: None,
        skipped_ids: None,
        error: Some("apply\u{1b}]0;spoof\u{7} failed\n".repeat(100)),
    }));
    assert_eq!(picker.prune_phase(), Some(PickerPrunePhase::Apply));
    assert_eq!(
        picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::PruneApplyRequested {
            preview: preview.clone(),
            generation: 11,
        }
    );
    let removed = preview.ids()[0..1].to_vec();
    let skipped = preview.ids()[1..].to_vec();
    assert!(picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview,
        removed_ids: Some(removed),
        skipped_ids: Some(skipped),
        error: None,
    }));
    assert!(picker.footer_text().contains("Removed 1"));
    assert!(picker.footer_text().contains("skipped 1"));
    assert_eq!(picker.resource_labels("build-box").unwrap(), initial_labels);
    assert_eq!(picker.generation(), 11);
}

#[test]
fn prune_integrity_failures_remain_retryable_and_generation_safe() {
    let mut mismatch_picker = prune_picker();
    mismatch_picker.handle(PickerEvent::BeginPrune);
    let mismatch_preview = prune_preview(14, 2);
    mismatch_picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(mismatch_preview.clone()),
    });
    mismatch_picker.handle(PickerEvent::ConfirmPrune);
    assert!(
        !mismatch_picker.apply_prune_result(PickerPruneResult::Apply {
            generation: 10,
            preview: mismatch_preview.clone(),
            removed_ids: Some(Vec::new()),
            skipped_ids: Some(mismatch_preview.ids().to_vec()),
            error: None,
        })
    );
    assert!(mismatch_picker.prune_busy());
    assert!(
        mismatch_picker.apply_prune_result(PickerPruneResult::Apply {
            generation: 11,
            preview: mismatch_preview.clone(),
            removed_ids: Some(Vec::new()),
            skipped_ids: Some(Vec::new()),
            error: None,
        })
    );
    assert!(matches!(
        mismatch_picker.prune_modal(),
        Some(PickerPruneModal::Failed {
            phase: PickerPrunePhase::Apply,
            preview: Some(preview),
            ..
        }) if preview == &mismatch_preview
    ));
    assert_eq!(
        mismatch_picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::PruneApplyRequested {
            preview: mismatch_preview,
            generation: 11,
        }
    );

    let mut incomplete_picker = prune_picker();
    incomplete_picker.handle(PickerEvent::BeginPrune);
    let incomplete_preview = prune_preview(14, 1);
    incomplete_picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(incomplete_preview.clone()),
    });
    incomplete_picker.handle(PickerEvent::ConfirmPrune);
    assert!(
        incomplete_picker.apply_prune_result(PickerPruneResult::Apply {
            generation: 11,
            preview: incomplete_preview.clone(),
            removed_ids: None,
            skipped_ids: None,
            error: None,
        })
    );
    assert!(matches!(
        incomplete_picker.prune_modal(),
        Some(PickerPruneModal::Failed {
            phase: PickerPrunePhase::Apply,
            preview: Some(preview),
            ..
        }) if preview == &incomplete_preview
    ));

    let mut retention_picker = prune_picker();
    retention_picker.handle(PickerEvent::BeginPrune);
    assert!(
        retention_picker.apply_prune_result(PickerPruneResult::Preview {
            generation: 11,
            result: Ok(prune_preview(13, 1)),
        })
    );
    assert!(!retention_picker.prune_busy());
    assert_eq!(
        retention_picker.prune_phase(),
        Some(PickerPrunePhase::Preview)
    );
    assert_eq!(
        retention_picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::PrunePreviewRequested {
            older_than_days: 14,
            generation: 11,
        }
    );
}

#[test]
fn confirmed_prune_result_survives_defensive_status_generation_change() {
    let mut picker = prune_picker();
    picker.handle(PickerEvent::BeginPrune);
    let preview = prune_preview(14, 1);
    picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview.clone()),
    });
    picker.handle(PickerEvent::ConfirmPrune);
    picker.begin_refresh(12);

    assert!(picker.apply_prune_result(PickerPruneResult::Apply {
        generation: 11,
        preview: preview.clone(),
        removed_ids: Some(preview.ids().to_vec()),
        skipped_ids: Some(Vec::new()),
        error: None,
    }));
    assert!(!picker.prune_busy());
    assert!(picker.footer_text().contains("Removed 1"));
}

#[test]
fn prune_and_close_modal_inputs_never_cross_route() {
    let (mut close_picker, _, _) = owned_close_picker();
    close_picker.handle(PickerEvent::Close);
    assert_eq!(
        close_picker.handle(PickerEvent::ConfirmPrune),
        PickerOutcome::Continue
    );
    assert!(close_picker.close_modal().is_some());

    let mut prune_picker = prune_picker();
    prune_picker.handle(PickerEvent::BeginPrune);
    let preview = prune_preview(14, 1);
    prune_picker.apply_prune_result(PickerPruneResult::Preview {
        generation: 11,
        result: Ok(preview),
    });
    assert_eq!(
        prune_picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::Continue
    );
    assert!(prune_picker.prune_modal().is_some());
}

#[cfg(unix)]
#[test]
fn herdr_inspection_rejects_oversized_process_output() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    fs::write(&binary, "#!/bin/sh\nhead -c 70000 /dev/zero\nexit 0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();

    let error = HerdrClient::new(context(&binary))
        .inspect_replacement_source()
        .unwrap_err();
    assert!(format!("{error:#}").contains("safe capture limit"));
}
