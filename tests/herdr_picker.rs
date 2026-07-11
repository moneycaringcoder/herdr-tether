use std::{fs, io::Write, path::Path, sync::Mutex, time::SystemTime};

use chrono::{Duration, TimeZone, Utc};
use herdr_tether::{
    backend::CommandSpec,
    config::{CommandPreset, Config, DiscoveryDefaults, HostConfig, RetentionDefaults, UiDefaults},
    discovery::{DiscoveryCompletion, DiscoveryMessage},
    herdr::{HerdrClient, HerdrContext},
    lifecycle::{CloseOwnedError, PrunePreview, PruneService},
    model::{ExternalSessionName, Placement, SessionId},
    state::{SessionRecord, SessionStatus, State, StateStore},
    status::{
        ExternalCatalogStatus, ExternalSession, HostReachability, StatusMessage, WorkloadStatus,
    },
    tui::{
        PickerCloseModal, PickerCloseResult, PickerEvent, PickerInput, PickerOptions,
        PickerOutcome, PickerPruneModal, PickerPrunePhase, PickerPruneResult, PickerSelection,
        PickerStage, PickerState, format_close_error,
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
    assert_eq!(picker.frame_title(), "Confirm close");
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
        }
    );
    assert!(picker.close_busy());
    assert_eq!(picker.frame_title(), "Closing workload");
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
    picker.handle(PickerEvent::DismissClose);

    picker.begin_refresh(8);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: owned_id })
    );
    picker.handle(PickerEvent::DismissClose);

    picker.handle(PickerEvent::Next);
    picker.handle(PickerEvent::Next);
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert!(picker.close_modal().is_none());
}

#[test]
fn close_success_removes_only_exact_row_and_chooses_deterministic_neighbor() {
    let (mut picker, first_id, second_id) = owned_close_picker();
    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested {
            id: first_id,
            generation: 7,
        }
    );
    assert!(picker.apply_close_result(PickerCloseResult {
        id: first_id,
        generation: 7,
        error: None,
    }));

    let labels = picker.resource_labels("build-box").unwrap();
    assert_eq!(labels.len(), 2);
    assert!(labels[0].contains("00000001"));
    assert_eq!(labels[1], "Create new Tether workload");
    assert_eq!(picker.handle(PickerEvent::Close), PickerOutcome::Continue);
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Confirm { id: second_id })
    );
    assert!(!picker.apply_close_result(PickerCloseResult {
        id: first_id,
        generation: 7,
        error: None,
    }));
}

#[test]
fn close_failure_is_sanitized_persistence_neutral_non_resumable_and_retryable() {
    let (mut picker, id, _) = owned_close_picker();
    picker.handle(PickerEvent::Close);
    picker.handle(PickerEvent::ConfirmClose);
    assert!(picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: Some("backend\u{1b}[31m failed\nretry".into()),
    }));
    assert_eq!(
        picker.close_modal(),
        Some(&PickerCloseModal::Failed {
            id,
            error: "backend failed retry".into(),
        })
    );
    assert_eq!(picker.frame_title(), "Close failed");
    assert!(picker.footer_text().starts_with("y retry · n/Esc dismiss"));
    assert!(
        picker.resource_labels("build-box").unwrap()[0].starts_with("[close failed · c retry]")
    );
    picker.handle(PickerEvent::DismissClose);
    assert_eq!(picker.handle(PickerEvent::Confirm), PickerOutcome::Continue);
    assert_eq!(picker.stage(), PickerStage::Resource);

    picker.handle(PickerEvent::Close);
    assert_eq!(
        picker.handle(PickerEvent::ConfirmClose),
        PickerOutcome::CloseOwnedRequested { id, generation: 7 }
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
fn refresh_rejects_late_close_generation() {
    let (mut picker, id, _) = owned_close_picker();
    picker.handle(PickerEvent::Close);
    picker.handle(PickerEvent::ConfirmClose);
    picker.begin_refresh(8);
    assert!(!picker.apply_close_result(PickerCloseResult {
        id,
        generation: 7,
        error: None,
    }));
    assert_eq!(picker.resource_labels("build-box").unwrap().len(), 3);
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
            id: format!("tether-0197f198000070008000000000000{:03}", index + 100)
                .parse()
                .unwrap(),
            host: "archived".into(),
            target: "nobody@example.test".into(),
            directory: "/closed".into(),
            preset: None,
            status: SessionStatus::Closed,
            created_at: now - Duration::days(60),
            last_used_at: now - Duration::days(40),
            closed_at: Some(now - Duration::days(40)),
        })
        .collect();
    store
        .save(&State {
            version: State::CURRENT_VERSION,
            sessions,
        })
        .unwrap();
    PruneService::new(store).preview(days).unwrap()
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
    assert!(before.contains("P prune closed"));
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
