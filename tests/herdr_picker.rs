use std::{fs, io::Write, path::Path, sync::Mutex, time::SystemTime};

use chrono::{Duration, TimeZone, Utc};
use herdr_tether::{
    backend::CommandSpec,
    config::{CommandPreset, Config, DiscoveryDefaults, HostConfig, RetentionDefaults, UiDefaults},
    discovery::{DiscoveryCompletion, DiscoveryMessage},
    herdr::{HerdrClient, HerdrContext},
    model::{ExternalSessionName, Placement, SessionId},
    state::{SessionRecord, SessionStatus, State},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, StatusMessage, WorkloadStatus,
    },
    tui::{
        PickerEvent, PickerInput, PickerOptions, PickerOutcome, PickerSelection, PickerStage,
        PickerState,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static FAKE_HERDR_LOCK: Mutex<()> = Mutex::new(());

fn write_fake_herdr(path: &Path, log: &Path) {
    let script = format!(
        r#"#!/bin/sh
printf 'CALL' >> '{log}'
for arg do printf '\t%s' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
if [ "$1 $2" = "pane split" ]; then
  printf '%s' '{{"id":"cli-1","result":{{"type":"pane_split","pane":{{"pane_id":"w1:p9","workspace_id":"w1","tab_id":"w1:t1"}}}}}}'
elif [ "$1 $2" = "tab create" ]; then
  printf '%s' '{{"id":"cli-2","result":{{"type":"tab_created","tab":{{"tab_id":"w1:t9","workspace_id":"w1"}},"root_pane":{{"pane_id":"w1:p10","workspace_id":"w1","tab_id":"w1:t9"}}}}}}'
elif [ "$1 $2" = "pane run" ]; then
  printf '%s' '{{"id":"cli-3","result":{{"type":"pane_ran","pane_id":"'$3'"}}}}'
elif [ "$1 $2 $3" = "plugin pane open" ]; then
  printf '%s' '{{"id":"cli-4","result":{{"type":"plugin_pane_opened","plugin_pane":{{"pane":{{"pane_id":"w1:p11"}}}}}}}}'
else
  printf '%s' '{{"id":"bad","error":{{"message":"unexpected fake invocation"}}}}'
  exit 2
fi
"#,
        log = log.display()
    );
    let mut file = fs::File::create(path).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn context(binary: &Path) -> HerdrContext {
    HerdrContext {
        binary: binary.into(),
        pane_id: "w1:p1".into(),
        workspace_id: "w1".into(),
    }
}

#[test]
fn placement_parses_returned_ids_and_runs_one_quoted_command_argument() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log);
    let client = HerdrClient::new(context(&binary));
    let command = CommandSpec {
        program: "/tmp/plugin root/herdr-tether".into(),
        args: vec![
            "session".into(),
            "resume".into(),
            "tether-0197f198000070008000000000000001".into(),
        ],
    };

    let right = client.place(&command, Placement::SplitRight).unwrap();
    assert_eq!(right.pane_id, "w1:p9");
    let down = client.place(&command, Placement::SplitDown).unwrap();
    assert_eq!(down.pane_id, "w1:p9");
    let tab = client.place(&command, Placement::NewTab).unwrap();
    assert_eq!(tab.pane_id, "w1:p10");

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("CALL\tpane\tsplit\t--pane\tw1:p1\t--direction\tright\t--focus"));
    assert!(transcript.contains("CALL\tpane\tsplit\t--pane\tw1:p1\t--direction\tdown\t--focus"));
    assert!(transcript.contains("CALL\ttab\tcreate\t--workspace\tw1"));
    assert!(transcript.contains(
        "CALL\tpane\trun\tw1:p9\t'/tmp/plugin root/herdr-tether' 'session' 'resume' 'tether-0197f198000070008000000000000001'"
    ));
}

#[test]
fn plugin_action_opens_the_declared_overlay_entrypoint() {
    let _guard = FAKE_HERDR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempdir().unwrap();
    let binary = temp.path().join("herdr");
    let log = temp.path().join("herdr.log");
    write_fake_herdr(&binary, &log);
    let client = HerdrClient::new(context(&binary));

    client.open_plugin_pane("picker").unwrap();

    assert!(fs::read_to_string(log).unwrap().contains(
        "CALL\tplugin\tpane\topen\t--plugin\tmoneycaringcoder.tether\t--entrypoint\tpicker\t--placement\toverlay"
    ));
}

fn picker_fixture() -> (Config, State) {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let config = Config {
        version: Config::CURRENT_VERSION,
        hosts: vec![HostConfig {
            name: "build-box".into(),
            target: "builder@example.test".into(),
            roots: vec!["/srv/configured".into(), "/srv/shared".into()],
            presets: vec![CommandPreset {
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
                id: "tether-0197f198000070008000000000000001"
                    .parse::<SessionId>()
                    .unwrap(),
                host: "build-box".into(),
                target: "builder@example.test".into(),
                directory: "/srv/shared".into(),
                preset: Some("agent".into()),
                status: SessionStatus::Active,
                created_at: now - Duration::days(2),
                last_used_at: now - Duration::hours(2),
                closed_at: None,
            },
            SessionRecord {
                id: "tether-0197f198000070008000000000000002"
                    .parse::<SessionId>()
                    .unwrap(),
                host: "build-box".into(),
                target: "builder@example.test".into(),
                directory: "/srv/recent".into(),
                preset: None,
                status: SessionStatus::Active,
                created_at: now - Duration::days(1),
                last_used_at: now - Duration::hours(1),
                closed_at: None,
            },
        ],
    };
    (config, state)
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
    closed.status = SessionStatus::Closed;
    closed.closed_at = Some(closed.last_used_at);
    state.sessions.push(closed);

    let options = PickerOptions::from_config_state(&config, &state, "/home/user", false);
    let build_box = options
        .hosts
        .iter()
        .find(|host| host.name == "build-box")
        .unwrap();
    assert_eq!(build_box.workloads.len(), 2);
    assert!(
        build_box.workloads[0]
            .label
            .starts_with("Tether · Resume …00000002 · Shell · ")
    );
    assert!(
        build_box
            .workloads
            .iter()
            .all(|workload| workload.id.to_string() != "tether-0197f198000070008000000000000003")
    );

    let expected_id = build_box.workloads[0].id;
    let mut explorer = PickerState::new(options).unwrap();
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
    assert!(labels[0].contains("Resume …00000002"));
    assert!(labels[1].contains("Resume …00000001"));
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
            .starts_with("[running · 2 attached] Tether · Resume …00000002")
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
        checked_at: SystemTime::UNIX_EPOCH,
    }));
    assert_eq!(
        explorer.host_label("build-box"),
        Some("[timeout] build-box")
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
