use std::{
    fs::{self, OpenOptions},
    sync::{Arc, Barrier, mpsc},
    thread,
    time::Duration as StdDuration,
};

use chrono::{TimeZone, Utc};
use fs2::FileExt;
use herdr_tether::{
    config::{
        CommandPreset, Config, ConfigStore, DiscoveryDefaults, HerdrKeybindingInstall,
        HerdrKeybindingRollback, HerdrKeybindingStore, HostConfig, RetentionDefaults, UiDefaults,
    },
    model::{OrchestrationGroupId, OrchestrationTitle, Placement},
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, StateStore,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
};

fn sample_config() -> Config {
    Config {
        version: Config::CURRENT_VERSION,
        hosts: vec![HostConfig {
            name: "build-box".into(),
            target: "builder@example.test".into(),
            roots: vec!["/srv/code".into()],
            presets: vec![CommandPreset {
                name: "shell".into(),
                command: "exec ${SHELL:-/bin/sh} -l".into(),
            }],
        }],
        ui: UiDefaults {
            placement: Placement::SplitRight,
        },
        discovery: DiscoveryDefaults::default(),
        retention: RetentionDefaults::default(),
    }
}

#[test]
fn config_round_trips_atomically_with_private_permissions() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config").join("config.toml");
    let store = ConfigStore::new(path.clone());
    let config = sample_config();

    store.save(&config).unwrap();

    assert_eq!(store.load().unwrap(), config);
    assert!(path.exists());
    assert!(
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
        "atomic write must not leave temporary files"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn config_v0_is_migrated_and_rewritten_as_v2() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        r#"version = 0

[[hosts]]
name = "work"
target = "work.example.test"
roots = ["/work"]
"#,
    )
    .unwrap();
    let store = ConfigStore::new(path.clone());

    let migrated = store.load().unwrap();

    assert_eq!(
        migrated,
        Config {
            version: 2,
            hosts: vec![HostConfig {
                name: "work".into(),
                target: "work.example.test".into(),
                roots: vec!["/work".into()],
                presets: Vec::new(),
            }],
            ui: UiDefaults::default(),
            discovery: DiscoveryDefaults::default(),
            retention: RetentionDefaults::default(),
        }
    );
    assert!(fs::read_to_string(path).unwrap().contains("version = 2"));
}

#[test]
fn config_v1_is_migrated_with_exact_v2_defaults() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        r#"version = 1

[[hosts]]
name = "work"
target = "work.example.test"
roots = ["/work"]
presets = [{ name = "shell", command = "exec /bin/sh" }]

[ui]
placement = "split-down"
"#,
    )
    .unwrap();

    let migrated = ConfigStore::new(path.clone()).load().unwrap();

    assert_eq!(migrated.version, 2);
    assert_eq!(migrated.hosts[0].roots, ["/work"]);
    assert_eq!(migrated.hosts[0].presets[0].name, "shell");
    assert_eq!(migrated.ui.placement, Placement::SplitDown);
    assert_eq!(
        migrated.discovery,
        DiscoveryDefaults {
            local_roots: Vec::new(),
            max_depth: 4,
            max_entries: 4096,
            max_results: 64,
            timeout_seconds: 3,
            workers: 4,
        }
    );
    assert_eq!(migrated.retention, RetentionDefaults { closed_days: 30 });
    let rewritten = fs::read_to_string(path).unwrap();
    assert!(rewritten.contains("version = 2"));
    assert!(rewritten.contains("[discovery]"));
    assert!(rewritten.contains("[retention]"));
}

#[test]
fn every_config_schema_rejects_unknown_fields() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let store = ConfigStore::new(path.clone());
    let sources = [
        "version = 0\nhosts = []\nunknown = true\n",
        "version = 1\nhosts = []\nunknown = true\n\n[ui]\nplacement = \"split-right\"\n",
        "version = 2\nhosts = []\nunknown = true\n\n[ui]\nplacement = \"split-right\"\n\n[discovery]\nlocal_roots = []\nmax_depth = 4\nmax_entries = 4096\nmax_results = 64\ntimeout_seconds = 3\nworkers = 4\n\n[retention]\nclosed_days = 30\n",
    ];

    for source in sources {
        fs::write(&path, source).unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(
            error.contains("decode config version"),
            "unexpected strict-schema error: {error}"
        );
    }
}

#[test]
fn config_v2_round_trip_and_limit_validation() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let store = ConfigStore::new(path.clone());
    let mut config = sample_config();
    config.discovery.local_roots = vec!["/srv/one".into(), "/srv/two".into()];
    config.discovery.max_depth = 8;
    config.discovery.max_entries = 8192;
    config.discovery.max_results = 128;
    config.discovery.timeout_seconds = 9;
    config.discovery.workers = 2;
    config.retention.closed_days = 45;

    store.save(&config).unwrap();
    assert_eq!(store.load().unwrap(), config);

    for field in [
        "max_depth",
        "max_entries",
        "max_results",
        "timeout_seconds",
        "workers",
        "closed_days",
    ] {
        let mut invalid = config.clone();
        match field {
            "max_depth" => invalid.discovery.max_depth = 0,
            "max_entries" => invalid.discovery.max_entries = 0,
            "max_results" => invalid.discovery.max_results = 0,
            "timeout_seconds" => invalid.discovery.timeout_seconds = 0,
            "workers" => invalid.discovery.workers = 0,
            "closed_days" => invalid.retention.closed_days = 0,
            _ => unreachable!(),
        }
        assert!(
            invalid.validate().unwrap_err().to_string().contains(field),
            "{field} should identify its validation error"
        );
    }

    let mut excessive_timeout = config.clone();
    excessive_timeout.discovery.timeout_seconds = 3601;
    assert!(
        excessive_timeout
            .validate()
            .unwrap_err()
            .to_string()
            .contains("at most 3600"),
        "operational timeout must not overflow or stall the explorer indefinitely"
    );

    let mut too_large = config.clone();
    too_large.discovery.max_depth = usize::MAX;
    assert!(
        too_large
            .validate()
            .unwrap_err()
            .to_string()
            .contains("maximum TOML integer")
    );
    too_large.discovery.max_depth = 1;
    too_large.discovery.timeout_seconds = u64::MAX;
    assert!(
        too_large
            .validate()
            .unwrap_err()
            .to_string()
            .contains("maximum TOML integer")
    );
    too_large.discovery.timeout_seconds = 1;
    too_large.retention.closed_days = u64::MAX;
    assert!(
        too_large
            .validate()
            .unwrap_err()
            .to_string()
            .contains("maximum TOML integer")
    );

    let mut invalid_root = config;
    invalid_root.discovery.local_roots.push(" \t".into());
    assert!(
        invalid_root
            .validate()
            .unwrap_err()
            .to_string()
            .contains("local root")
    );
}

#[test]
fn concurrent_config_updates_preserve_both_hosts() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();

    for name in ["one", "two"] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            ConfigStore::new(path)
                .update(|config| {
                    thread::sleep(StdDuration::from_millis(50));
                    config.add_host(HostConfig {
                        name: name.into(),
                        target: format!("{name}.example.test"),
                        roots: vec![format!("/{name}")],
                        presets: Vec::new(),
                    })
                })
                .unwrap();
        }));
    }

    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }

    let config = ConfigStore::new(path).load().unwrap();
    assert_eq!(config.hosts.len(), 2);
    assert!(config.hosts.iter().any(|host| host.name == "one"));
    assert!(config.hosts.iter().any(|host| host.name == "two"));
}

fn assert_load_waits_for_lock(path: &std::path::Path, load: impl FnOnce() + Send + 'static) {
    let lock_path = path.parent().unwrap().join(format!(
        ".{}.lock",
        path.file_name().unwrap().to_string_lossy()
    ));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        load();
        done_tx.send(()).unwrap();
    });

    assert!(
        done_rx.recv_timeout(StdDuration::from_millis(100)).is_err(),
        "load-time migration must wait for the advisory lock"
    );
    FileExt::unlock(&lock).unwrap();
    done_rx.recv_timeout(StdDuration::from_secs(2)).unwrap();
    worker.join().unwrap();
}

#[test]
fn config_load_time_migration_holds_the_advisory_lock() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "version = 1\nhosts = []\n\n[ui]\nplacement = \"split-right\"\n",
    )
    .unwrap();
    let load_path = path.clone();

    assert_load_waits_for_lock(&path, move || {
        ConfigStore::new(load_path).load().unwrap();
    });

    assert!(fs::read_to_string(path).unwrap().contains("version = 2"));
}

#[test]
fn invalid_config_is_rejected_before_persistence() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let store = ConfigStore::new(path.clone());
    let mut config = sample_config();
    config.hosts.push(config.hosts[0].clone());

    let error = store.save(&config).unwrap_err().to_string();

    assert!(error.contains("duplicate host name"));
    assert!(!path.exists());
}

#[test]
fn state_round_trips_and_migrates_v0() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let store = StateStore::new(path.clone());
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: vec![SessionRecord {
            id: "tether-0197f198000070008000000000000001".parse().unwrap(),
            host: "build-box".into(),
            target: "builder@example.test".into(),
            directory: "/srv/code".into(),
            preset: Some("shell".into()),
            command: Some("exec shell".into()),
            tmux_session_id: Some("$7".parse().unwrap()),
            ownership_proof: None,
            status: SessionStatus::Running,
            created_at: now,
            last_used_at: now,
            closed_at: None,
            exit_status: None,
        }],
        orchestration_groups: Vec::new(),
    };

    store.save(&state).unwrap();
    assert_eq!(store.load().unwrap(), state);
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fs::write(
        &path,
        r#"{"version":0,"sessions":[{"id":"tether-0197f198000070008000000000000001","host":"build-box","target":"builder@example.test","directory":"/srv/code","created_at":"2026-07-10T12:00:00Z","last_used_at":"2026-07-10T12:00:00Z"}]}"#,
    )
    .unwrap();
    let migrated = store.load().unwrap();
    assert_eq!(migrated.version, State::CURRENT_VERSION);
    assert_eq!(migrated.sessions[0].status, SessionStatus::Running);
    assert!(migrated.sessions[0].ownership_proof.is_none());
    assert!(migrated.sessions[0].preset.is_none());
    assert!(migrated.orchestration_groups.is_empty());
    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 3"));
}

#[test]
fn state_v2_migration_preserves_sessions_and_creates_no_groups() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    fs::write(
        &path,
        r#"{"version":2,"sessions":[{"id":"tether-0197f198000070008000000000000001","host":"build-box","target":"builder@example.test","directory":"/srv/code","preset":"shell","command":"exec shell","tmux_session_id":7,"ownership_proof":null,"status":"running","created_at":"2026-07-10T12:00:00Z","last_used_at":"2026-07-10T12:00:00Z","closed_at":null,"exit_status":null}]}"#,
    )
    .unwrap();

    let migrated = StateStore::new(path.clone()).load().unwrap();

    assert_eq!(migrated.version, 3);
    assert_eq!(migrated.sessions.len(), 1);
    assert_eq!(migrated.sessions[0].host, "build-box");
    assert_eq!(migrated.sessions[0].target, "builder@example.test");
    assert_eq!(migrated.sessions[0].directory, "/srv/code");
    assert_eq!(migrated.sessions[0].preset.as_deref(), Some("shell"));
    assert_eq!(migrated.sessions[0].command.as_deref(), Some("exec shell"));
    assert_eq!(
        migrated.sessions[0].tmux_session_id,
        Some("$7".parse().unwrap())
    );
    assert!(migrated.sessions[0].ownership_proof.is_none());
    assert_eq!(migrated.sessions[0].status, SessionStatus::Running);
    assert!(migrated.orchestration_groups.is_empty());
    let rewritten = fs::read_to_string(path).unwrap();
    assert!(rewritten.contains("\"version\": 3"));
    assert!(rewritten.contains("\"orchestration_groups\": []"));
}

#[test]
fn future_state_version_fails_closed_without_rewriting_data() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let source = br#"{"version":4,"sessions":[],"orchestration_groups":[]}"#;
    fs::write(&path, source).unwrap();

    let error = StateStore::new(path.clone())
        .load()
        .unwrap_err()
        .to_string();

    assert!(error.contains("unsupported state version 4"), "{error}");
    assert_eq!(fs::read(path).unwrap(), source);
}

#[test]
fn state_v3_orchestration_groups_round_trip_arbitrary_references() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let orchestrator = "tether-0197f198000070008000000000000010".parse().unwrap();
    let observer = "tether-0197f198000070008000000000000011".parse().unwrap();
    let interactive = "tether-0197f198000070008000000000000012".parse().unwrap();
    let both = "tether-0197f198000070008000000000000013".parse().unwrap();
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: Vec::new(),
        orchestration_groups: vec![OrchestrationGroup {
            id: "generic-build".parse().unwrap(),
            title: "Generic Build".parse().unwrap(),
            orchestrator_session_id: orchestrator,
            workers: vec![
                OrchestrationMember {
                    session_id: observer,
                    title: None,
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: false,
                    },
                },
                OrchestrationMember {
                    session_id: interactive,
                    title: Some("Interactive worker".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: false,
                        open_interactive: true,
                    },
                },
                OrchestrationMember {
                    session_id: both,
                    title: Some("Both capabilities".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: true,
                    },
                },
            ],
        }],
    };

    let store = StateStore::new(path);
    store.save(&state).unwrap();
    assert_eq!(store.load().unwrap(), state);
}

#[test]
fn orchestration_identifiers_and_titles_are_safe_and_bounded() {
    for invalid in [
        "",
        "Uppercase",
        "-leading",
        "trailing-",
        "two--separators",
        "contains space",
        "contains/slash",
        &"a".repeat(OrchestrationGroupId::MAX_BYTES + 1),
    ] {
        assert!(
            invalid.parse::<OrchestrationGroupId>().is_err(),
            "{invalid:?}"
        );
    }
    for valid in [
        "a",
        "generic-build_2",
        &"a".repeat(OrchestrationGroupId::MAX_BYTES),
    ] {
        assert!(valid.parse::<OrchestrationGroupId>().is_ok(), "{valid:?}");
    }
    for invalid in [
        "",
        " leading",
        "trailing ",
        "line\nbreak",
        "escape\u{1b}",
        "non\u{00a0}ascii",
        &"a".repeat(OrchestrationTitle::MAX_BYTES + 1),
    ] {
        assert!(
            invalid.parse::<OrchestrationTitle>().is_err(),
            "{invalid:?}"
        );
    }
}

#[test]
fn state_v3_orchestration_validation_fails_closed() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let store = StateStore::new(path);
    let session = |suffix: usize| {
        format!("tether-0197f19800007000800000000000{suffix:04}")
            .parse()
            .unwrap()
    };
    let group = |id: &str| OrchestrationGroup {
        id: id.parse().unwrap(),
        title: "Group".parse().unwrap(),
        orchestrator_session_id: session(1),
        workers: vec![OrchestrationMember {
            session_id: session(2),
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
            },
        }],
    };
    let mut state = State {
        version: State::CURRENT_VERSION,
        sessions: Vec::new(),
        orchestration_groups: vec![group("one"), group("one")],
    };
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("duplicate orchestration group id")
    );

    state.orchestration_groups = (0..=State::MAX_ORCHESTRATION_GROUPS)
        .map(|index| group(&format!("group-{index}")))
        .collect();
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("at most")
    );

    state.orchestration_groups = vec![group("workers")];
    state.orchestration_groups[0].workers = (0..=OrchestrationGroup::MAX_WORKERS)
        .map(|index| OrchestrationMember {
            session_id: session(index + 10),
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
            },
        })
        .collect();
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("at most")
    );

    state.orchestration_groups = vec![group("duplicate-worker")];
    let duplicate_worker = state.orchestration_groups[0].workers[0].clone();
    state.orchestration_groups[0].workers.push(duplicate_worker);
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("duplicate worker session")
    );

    state.orchestration_groups = vec![group("orchestrator-worker")];
    state.orchestration_groups[0].workers[0].session_id =
        state.orchestration_groups[0].orchestrator_session_id;
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("orchestrator")
    );

    state.orchestration_groups = vec![group("no-capability")];
    state.orchestration_groups[0].workers[0].capabilities = OrchestrationCapabilities::default();
    assert!(
        store
            .save(&state)
            .unwrap_err()
            .to_string()
            .contains("capability")
    );
}

#[test]
fn state_v3_rejects_unsafe_orchestration_json() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let store = StateStore::new(path.clone());
    let orchestrator = "tether-0197f198000070008000000000000001";
    let worker = "tether-0197f198000070008000000000000002";
    let invalid_sources = [
        format!(
            r#"{{"version":3,"sessions":[],"orchestration_groups":[{{"id":"Bad ID","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[]}}]}}"#
        ),
        format!(
            r#"{{"version":3,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"line\nbreak","orchestrator_session_id":"{orchestrator}","workers":[]}}]}}"#
        ),
        format!(
            r#"{{"version":3,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[{{"session_id":"{worker}","title":" trailing ","capabilities":{{"observe_output":true,"open_interactive":false}}}}]}}]}}"#
        ),
        format!(
            r#"{{"version":3,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[{{"session_id":"{worker}","capabilities":{{"observe_output":true}}}}]}}]}}"#
        ),
    ];

    for source in invalid_sources {
        fs::write(&path, source).unwrap();
        assert!(store.load().is_err());
    }
}

#[test]
fn every_state_schema_rejects_unknown_fields() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let store = StateStore::new(path.clone());
    let id = "tether-0197f198000070008000000000000001";
    let timestamps = r#""created_at":"2026-07-10T12:00:00Z","last_used_at":"2026-07-10T12:00:00Z""#;
    let sources = [
        r#"{"version":0,"sessions":[],"unknown":true}"#.to_owned(),
        format!(
            r#"{{"version":0,"sessions":[{{"id":"{id}","host":"local","target":"local","directory":"/tmp",{timestamps},"unknown":true}}]}}"#
        ),
        r#"{"version":1,"sessions":[],"unknown":true}"#.to_owned(),
        format!(
            r#"{{"version":1,"sessions":[{{"id":"{id}","host":"local","target":"local","directory":"/tmp","preset":null,"status":"active",{timestamps},"closed_at":null,"unknown":true}}]}}"#
        ),
        r#"{"version":2,"sessions":[],"unknown":true}"#.to_owned(),
        format!(
            r#"{{"version":2,"sessions":[{{"id":"{id}","host":"local","target":"local","directory":"/tmp","preset":null,"command":null,"tmux_session_id":null,"ownership_proof":null,"status":"running",{timestamps},"closed_at":null,"exit_status":null,"unknown":true}}]}}"#
        ),
        r#"{"version":3,"sessions":[],"orchestration_groups":[],"unknown":true}"#.to_owned(),
        r#"{"version":3,"sessions":[],"orchestration_groups":[{"id":"valid","title":"Valid","orchestrator_session_id":"tether-0197f198000070008000000000000001","workers":[],"unknown":true}]}"#.to_owned(),
        r#"{"version":3,"sessions":[],"orchestration_groups":[{"id":"valid","title":"Valid","orchestrator_session_id":"tether-0197f198000070008000000000000001","workers":[{"session_id":"tether-0197f198000070008000000000000002","capabilities":{"observe_output":true,"open_interactive":false,"unknown":true}}]}]}"#.to_owned(),
    ];

    for source in sources {
        fs::write(&path, source).unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(
            error.contains("decode state version"),
            "unexpected strict-schema error: {error}"
        );
    }
}

#[test]
fn state_load_time_migration_holds_the_advisory_lock() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    fs::write(&path, r#"{"version":0,"sessions":[]}"#).unwrap();
    let load_path = path.clone();

    assert_load_waits_for_lock(&path, move || {
        StateStore::new(load_path).load().unwrap();
    });

    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 3"));
}

#[test]
fn concurrent_state_updates_preserve_both_records() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();

    for suffix in ["0001", "0002"] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            StateStore::new(path)
                .update(|state| {
                    thread::sleep(StdDuration::from_millis(50));
                    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
                    state.sessions.push(SessionRecord {
                        id: format!("tether-0197f19800007000800000000000{suffix}")
                            .parse()
                            .unwrap(),
                        host: "local".into(),
                        target: "local".into(),
                        directory: "/tmp".into(),
                        preset: None,
                        command: Some("exec shell".into()),
                        tmux_session_id: None,
                        ownership_proof: None,
                        status: SessionStatus::Running,
                        created_at: now,
                        last_used_at: now,
                        closed_at: None,
                        exit_status: None,
                    });
                    Ok(())
                })
                .unwrap();
        }));
    }

    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }

    let state = StateStore::new(path).load().unwrap();
    assert_eq!(state.sessions.len(), 2);
}

#[cfg(unix)]
#[test]
fn state_rejects_symlink_and_fifo_storage_paths() {
    let temp = tempdir().unwrap();
    let target = temp.path().join("target.json");
    fs::write(&target, r#"{"version":2,"sessions":[]}"#).unwrap();
    let symlink_path = temp.path().join("symlink-state.json");
    std::os::unix::fs::symlink(&target, &symlink_path).unwrap();

    let symlink_error = format!("{:#}", StateStore::new(symlink_path).load().unwrap_err());
    assert!(
        symlink_error.contains("symbolic link") || symlink_error.contains("regular file"),
        "unexpected symlink error: {symlink_error}"
    );
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        r#"{"version":2,"sessions":[]}"#
    );

    let fifo_path = temp.path().join("fifo-state.json");
    let fifo_c_path = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c_path.as_ptr(), 0o600) }, 0);
    let fifo_error = format!("{:#}", StateStore::new(fifo_path).load().unwrap_err());
    assert!(
        fifo_error.contains("regular file"),
        "unexpected FIFO error: {fifo_error}"
    );
}

#[cfg(unix)]
#[test]
fn replacing_lock_path_cannot_split_state_mutual_exclusion() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    StateStore::new(path.clone())
        .save(&State::default())
        .unwrap();
    let lock_path = temp.path().join(".state.json.lock");
    let displaced_lock_path = temp.path().join(".state.json.lock.displaced");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let update_path = path.clone();
    let updater = thread::spawn(move || {
        StateStore::new(update_path)
            .update(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    entered_rx.recv_timeout(StdDuration::from_secs(2)).unwrap();
    fs::rename(&lock_path, &displaced_lock_path).unwrap();

    let (load_done_tx, load_done_rx) = mpsc::channel();
    let load_path = path.clone();
    let loader = thread::spawn(move || {
        let result = StateStore::new(load_path).load();
        load_done_tx.send(result).unwrap();
    });
    assert!(
        load_done_rx
            .recv_timeout(StdDuration::from_millis(100))
            .is_err(),
        "replacement lock inode must not allow a concurrent state operation"
    );

    release_tx.send(()).unwrap();
    updater.join().unwrap();
    load_done_rx
        .recv_timeout(StdDuration::from_secs(2))
        .unwrap()
        .unwrap();
    loader.join().unwrap();
}

const TETHER_BINDING: &str = r#"[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "moneycaringcoder.tether.open"
description = "Tether: Open"
"#;

#[test]
fn herdr_keybinding_install_preserves_source_and_backup_bytes() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("herdr/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o750)).unwrap();
    let original = b"# keep this byte-for-byte\n[theme]\nname = \"custom\"\n";
    fs::write(&path, original).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

    let store = HerdrKeybindingStore::new(path.clone());
    let installed = store.install().unwrap();
    let backup = match installed {
        HerdrKeybindingInstall::Installed { backup } => backup,
        other => panic!("unexpected install result: {other:?}"),
    };

    let installed_bytes = fs::read(&path).unwrap();
    assert!(installed_bytes.starts_with(original));
    assert!(installed_bytes.ends_with(TETHER_BINDING.as_bytes()));
    assert_eq!(fs::read(&backup).unwrap(), original);
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
    }
}

#[test]
fn herdr_keybinding_install_is_idempotent_without_rewriting() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = format!("# exact\n{TETHER_BINDING}");
    fs::write(&path, &original).unwrap();

    let result = HerdrKeybindingStore::new(path.clone()).install().unwrap();

    assert_eq!(result, HerdrKeybindingInstall::AlreadyInstalled);
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
}

#[test]
fn herdr_keybinding_conflict_refuses_all_mutation_with_safe_diagnostic() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original =
        b"[[keys.command]]\nkey = \"prefix+t\"\ntype = \"command\"\ncommand = \"echo secret\"\n";
    fs::write(&path, original).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err()
        .to_string();

    assert!(error.contains("prefix+t"));
    assert!(error.contains("already bound"));
    assert!(!error.contains("echo secret"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
}

#[test]
fn herdr_keybinding_rollback_restores_backup_exactly() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"# original\r\nonboarding = false\r\n";
    fs::write(&path, original).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let store = HerdrKeybindingStore::new(path.clone());
    store.install().unwrap();

    let result = store.rollback().unwrap();

    assert_eq!(result, HerdrKeybindingRollback::Restored);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
    assert!(matches!(
        store.install().unwrap(),
        HerdrKeybindingInstall::Installed { .. }
    ));
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn herdr_keybinding_rejects_invalid_toml_without_creating_backup() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"[keys\n";
    fs::write(&path, original).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err()
        .to_string();

    assert!(error.contains("parse Herdr config"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
}

#[test]
fn herdr_keybinding_backup_collision_leaves_both_files_unchanged() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"onboarding = false\n";
    fs::write(&path, original).unwrap();
    let backup = HerdrKeybindingStore::backup_path_for(&path);
    let prior_backup = b"unrelated prior backup\n";
    fs::write(&backup, prior_backup).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err()
        .to_string();

    assert!(error.contains("backup"));
    assert!(error.contains("config was not changed"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read(&backup).unwrap(), prior_backup);
}

#[test]
fn herdr_keybinding_consumes_matching_stale_backup_before_retry() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"onboarding = false\n";
    fs::write(&path, original).unwrap();
    let backup = HerdrKeybindingStore::backup_path_for(&path);
    fs::write(&backup, original).unwrap();

    let result = HerdrKeybindingStore::new(path.clone()).install().unwrap();

    assert!(matches!(result, HerdrKeybindingInstall::Installed { .. }));
    assert!(
        fs::read(&path)
            .unwrap()
            .ends_with(TETHER_BINDING.as_bytes())
    );
}

#[test]
fn herdr_keybinding_rollback_refuses_to_overwrite_later_edits() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, b"onboarding = false\n").unwrap();
    let store = HerdrKeybindingStore::new(path.clone());
    store.install().unwrap();
    let edited = b"onboarding = true\n";
    fs::write(&path, edited).unwrap();

    let error = store.rollback().unwrap_err().to_string();

    assert!(error.contains("changed after Tether installed"));
    assert_eq!(fs::read(&path).unwrap(), edited);
    assert!(HerdrKeybindingStore::backup_path_for(&path).exists());
}

#[cfg(unix)]
#[test]
fn state_lock_refuses_symlink_without_touching_victim() {
    let temp = tempdir().unwrap();
    let state = temp.path().join("state.json");
    let lock = temp.path().join(".state.json.lock");
    let victim = temp.path().join("victim");
    fs::write(&victim, b"do not touch").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&victim, &lock).unwrap();

    let error = StateStore::new(state).load().unwrap_err().to_string();
    assert!(error.contains("open storage lock"));
    assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
    assert_eq!(
        fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn herdr_keybinding_rejects_unmergeable_toml_before_backup() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"keys = { prefix = \"ctrl+b\" }\n";
    fs::write(&path, original).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err()
        .to_string();

    assert!(error.contains("cannot be merged"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
}

#[test]
fn herdr_keybinding_detects_conflicts_in_builtin_action_fields() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"[keys]\nworkspace_picker = [\"prefix+w\", \"prefix+t\"]\n";
    fs::write(&path, original).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err()
        .to_string();

    assert!(error.contains("prefix+t"));
    assert!(error.contains("already bound"));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&path).exists());
}
