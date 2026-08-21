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
    model::{OrchestrationGroupId, OrchestrationTitle, Placement, SessionId},
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State, StateStore,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
};

fn sample_config() -> Config {
    Config {
        version: Config::CURRENT_VERSION,
        notifications: Default::default(),
        hosts: vec![HostConfig {
            name: "build-box".into(),
            target: "builder@example.test".into(),
            roots: vec!["/srv/code".into()],
            presets: vec![CommandPreset {
                herdr_agent: None,
                name: "shell".into(),
                command: "exec ${SHELL:-/bin/sh} -l".into(),
                health_command: None,
            }],
        }],
        ui: UiDefaults {
            placement: Placement::SplitRight,
        },
        discovery: DiscoveryDefaults::default(),
        retention: RetentionDefaults::default(),
    }
}

fn config_at_serialized_limit() -> Config {
    let mut config = Config {
        hosts: vec![HostConfig {
            name: "boundary".into(),
            target: "example.test".into(),
            roots: Vec::new(),
            presets: (0..33)
                .map(|index| CommandPreset {
                    herdr_agent: None,
                    name: format!("preset-{index}"),
                    command: "x".into(),
                    health_command: None,
                })
                .collect(),
        }],
        ..Config::default()
    };
    let current = toml::to_string_pretty(&config).unwrap().len();
    let mut remaining = ConfigStore::MAX_INPUT_BYTES - current;
    for preset in &mut config.hosts[0].presets {
        let added = remaining.min(CommandPreset::MAX_COMMAND_BYTES - preset.command.len());
        preset.command.push_str(&"x".repeat(added));
        remaining -= added;
    }
    assert_eq!(remaining, 0);
    assert_eq!(
        toml::to_string_pretty(&config).unwrap().len(),
        ConfigStore::MAX_INPUT_BYTES
    );
    config
}

fn state_at_serialized_limit() -> State {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let mut state = State {
        version: State::CURRENT_VERSION,
        sessions: (0..33)
            .map(|index| SessionRecord {
                herdr_agent: None,
                id: format!("tether-0197f19800007000800000000000{index:04x}")
                    .parse()
                    .unwrap(),
                host: "host".into(),
                target: "example.test".into(),
                directory: "/work".into(),
                preset: None,
                command: Some("x".into()),
                tmux_session_id: None,
                ownership_proof: None,
                status: SessionStatus::Running,
                created_at: now,
                last_used_at: now,
                closed_at: None,
                exit_status: None,
            })
            .collect(),
        orchestration_groups: Vec::new(),
    };
    let current = serde_json::to_string_pretty(&state).unwrap().len() + 1;
    let mut remaining = StateStore::MAX_INPUT_BYTES - current;
    for session in &mut state.sessions {
        let command = session.command.as_mut().unwrap();
        let added = remaining.min(State::MAX_COMMAND_BYTES - command.len());
        command.push_str(&"x".repeat(added));
        remaining -= added;
    }
    assert_eq!(remaining, 0);
    assert_eq!(
        serde_json::to_string_pretty(&state).unwrap().len() + 1,
        StateStore::MAX_INPUT_BYTES
    );
    state
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
fn config_v0_is_migrated_and_rewritten_as_current() {
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
            version: Config::CURRENT_VERSION,
            hosts: vec![HostConfig {
                name: "work".into(),
                target: "work.example.test".into(),
                roots: vec!["/work".into()],
                presets: Vec::new(),
            }],
            ui: UiDefaults::default(),
            notifications: Default::default(),
            discovery: DiscoveryDefaults::default(),
            retention: RetentionDefaults::default(),
        }
    );
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains(&format!("version = {}", Config::CURRENT_VERSION))
    );
}

#[test]
fn config_v1_is_migrated_with_current_defaults() {
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

    assert_eq!(migrated.version, Config::CURRENT_VERSION);
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
    assert!(rewritten.contains(&format!("version = {}", Config::CURRENT_VERSION)));
    assert!(rewritten.contains("[discovery]"));
    assert!(rewritten.contains("[retention]"));
}

#[test]
fn config_v2_is_migrated_with_an_empty_agent_hint() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        r#"version = 2

[[hosts]]
name = "work"
target = "work.example.test"
roots = ["/work"]
presets = [{ name = "shell", command = "exec /bin/sh" }]

[ui]
placement = "split-right"

[discovery]
local_roots = []
max_depth = 4
max_entries = 4096
max_results = 64
timeout_seconds = 3
workers = 4

[retention]
closed_days = 30
"#,
    )
    .unwrap();

    let migrated = ConfigStore::new(path.clone()).load().unwrap();

    assert_eq!(migrated.version, Config::CURRENT_VERSION);
    assert_eq!(migrated.hosts[0].presets[0].herdr_agent, None);
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains(&format!("version = {}", Config::CURRENT_VERSION))
    );
}

#[test]
fn read_only_config_migration_preserves_legacy_bytes() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let source = b"version = 1\nhosts = []\n\n[ui]\nplacement = \"split-right\"\n";
    fs::write(&path, source).unwrap();

    let migrated = ConfigStore::new(path.clone()).load_read_only().unwrap();

    assert_eq!(migrated.version, Config::CURRENT_VERSION);
    assert_eq!(fs::read(path).unwrap(), source);
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

    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains(&format!("version = {}", Config::CURRENT_VERSION))
    );
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
fn config_cardinality_and_string_boundaries_are_enforced_before_persistence() {
    let boundary = "x".repeat(Config::MAX_STRING_BYTES);
    let mut cases: Vec<(&str, Config, Config)> = Vec::new();

    let host_count = Config {
        hosts: (0..Config::MAX_HOSTS)
            .map(|index| HostConfig {
                name: format!("host-{index}"),
                target: "example.test".into(),
                roots: Vec::new(),
                presets: Vec::new(),
            })
            .collect(),
        ..Config::default()
    };
    let mut host_count_over = host_count.clone();
    host_count_over.hosts.push(HostConfig {
        name: "host-over-limit".into(),
        target: "example.test".into(),
        roots: Vec::new(),
        presets: Vec::new(),
    });
    cases.push(("config hosts", host_count, host_count_over));

    let mut roots = sample_config();
    roots.hosts[0].roots = vec!["/root".into(); HostConfig::MAX_ROOTS];
    let mut roots_over = roots.clone();
    roots_over.hosts[0].roots.push("/over".into());
    cases.push(("host roots", roots, roots_over));

    let mut presets = sample_config();
    presets.hosts[0].presets = (0..HostConfig::MAX_PRESETS)
        .map(|index| CommandPreset {
            herdr_agent: None,
            name: format!("preset-{index}"),
            command: "true".into(),
            health_command: None,
        })
        .collect();
    let mut presets_over = presets.clone();
    presets_over.hosts[0].presets.push(CommandPreset {
        herdr_agent: None,
        name: "over".into(),
        command: "true".into(),
        health_command: None,
    });
    cases.push(("host presets", presets, presets_over));

    let mut local_roots = sample_config();
    local_roots.discovery.local_roots = vec!["/root".into(); DiscoveryDefaults::MAX_LOCAL_ROOTS];
    let mut local_roots_over = local_roots.clone();
    local_roots_over.discovery.local_roots.push("/over".into());
    cases.push(("discovery local roots", local_roots, local_roots_over));

    for field in ["name", "target", "root", "preset name"] {
        let mut valid = sample_config();
        match field {
            "name" => valid.hosts[0].name = boundary.clone(),
            "target" => valid.hosts[0].target = boundary.clone(),
            "root" => valid.hosts[0].roots[0] = boundary.clone(),
            "preset name" => valid.hosts[0].presets[0].name = boundary.clone(),
            _ => unreachable!(),
        }
        let mut invalid = valid.clone();
        match field {
            "name" => invalid.hosts[0].name.push('x'),
            "target" => invalid.hosts[0].target.push('x'),
            "root" => invalid.hosts[0].roots[0].push('x'),
            "preset name" => invalid.hosts[0].presets[0].name.push('x'),
            _ => unreachable!(),
        }
        cases.push((field, valid, invalid));
    }

    let mut local_root = sample_config();
    local_root.discovery.local_roots = vec![boundary.clone()];
    let mut local_root_over = local_root.clone();
    local_root_over.discovery.local_roots[0].push('x');
    cases.push(("discovery local root", local_root, local_root_over));

    let command_boundary = "x".repeat(CommandPreset::MAX_COMMAND_BYTES);
    let mut command = sample_config();
    command.hosts[0].presets[0].command = command_boundary;
    let mut command_over = command.clone();
    command_over.hosts[0].presets[0].command.push('x');
    cases.push(("preset command", command, command_over));

    // A health command is trusted code held to the same standard as the command
    // it checks, so an empty or oversized probe is a configuration mistake.
    let mut health = sample_config();
    health.hosts[0].presets[0].health_command = Some("x".repeat(CommandPreset::MAX_COMMAND_BYTES));
    let mut health_over = health.clone();
    health_over.hosts[0].presets[0]
        .health_command
        .as_mut()
        .unwrap()
        .push('x');
    cases.push(("preset health command", health, health_over));

    let mut empty_health = sample_config();
    empty_health.hosts[0].presets[0].health_command = Some(String::new());
    let error = empty_health.validate().unwrap_err().to_string();
    assert!(
        error.contains("health command must not be empty"),
        "an empty probe must be rejected rather than run: {error}"
    );

    for (category, valid, invalid) in cases {
        valid
            .validate()
            .unwrap_or_else(|error| panic!("{category} boundary rejected: {error}"));
        let error = invalid.validate().unwrap_err().to_string();
        assert!(
            error.contains(category),
            "unexpected {category} error: {error}"
        );

        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let store = ConfigStore::new(path.clone());
        store.save(&sample_config()).unwrap();
        let before = fs::read(&path).unwrap();
        let save_error = store.save(&invalid).unwrap_err().to_string();
        assert!(
            save_error.contains(category),
            "unexpected persisted {category} error: {save_error}"
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn config_serialized_ceiling_round_trips_current_and_migrated_bytes() {
    let config = config_at_serialized_limit();
    for migrated in [false, true] {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let store = ConfigStore::new(path.clone());
        if migrated {
            let mut legacy = toml::Value::try_from(&config).unwrap();
            let table = legacy.as_table_mut().unwrap();
            table.insert("version".into(), 1.into());
            table.remove("discovery");
            table.remove("retention");
            // v1 predates opt-in notifications and rejects unknown fields.
            table.remove("notifications");
            let source = toml::to_string(&legacy).unwrap();
            assert!(source.len() <= ConfigStore::MAX_INPUT_BYTES);
            fs::write(&path, source).unwrap();
            assert_eq!(store.load().unwrap(), config);
        } else {
            store.save(&config).unwrap();
        }
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), ConfigStore::MAX_INPUT_BYTES);
        assert_eq!(store.load().unwrap(), config);
        store.save(&config).unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn a_config_written_before_a_notification_setting_existed_keeps_the_documented_default() {
    let config = sample_config();
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let store = ConfigStore::new(path.clone());
    let mut written = toml::Value::try_from(&config).unwrap();
    written
        .as_table_mut()
        .unwrap()
        .get_mut("notifications")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .remove("workload_failed");
    fs::write(&path, toml::to_string(&written).unwrap()).unwrap();

    // A setting the file predates must not read as "off". Opting out has to be
    // something the user wrote, not something the file's age decided.
    let loaded = store.load().unwrap();
    assert!(loaded.notifications.workload_failed);
    assert_eq!(loaded, config);
}

#[test]
fn config_serialized_n_plus_one_is_rejected_without_changing_file() {
    let mut config = config_at_serialized_limit();
    config.hosts[0]
        .presets
        .last_mut()
        .unwrap()
        .command
        .push('x');
    config.validate().unwrap();
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let before = b"existing config bytes";
    fs::write(&path, before).unwrap();

    let error = ConfigStore::new(path.clone())
        .save(&config)
        .unwrap_err()
        .to_string();

    assert!(error.contains("serialized config"), "{error}");
    assert!(error.contains("at most"), "{error}");
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn oversized_config_input_is_rejected_before_toml_parsing() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, vec![b'x'; ConfigStore::MAX_INPUT_BYTES + 1]).unwrap();

    let error = ConfigStore::new(path).load().unwrap_err().to_string();
    assert!(error.contains("config input"));
    assert!(error.contains("at most"));
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
            herdr_agent: None,
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
    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 4"));
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

    assert_eq!(migrated.version, State::CURRENT_VERSION);
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
    assert!(rewritten.contains("\"version\": 4"));
    assert!(rewritten.contains("\"orchestration_groups\": []"));
}

#[test]
fn state_v3_migration_assigns_and_persists_membership_epochs() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let source = r#"{"version":3,"sessions":[],"orchestration_groups":[{"id":"group","title":"Group","orchestrator_session_id":"tether-0197f198000070008000000000000001","workers":[{"session_id":"tether-0197f198000070008000000000000002","capabilities":{"observe_output":true,"open_interactive":false}}]}]}"#;
    fs::write(&path, source).unwrap();
    let store = StateStore::new(path.clone());

    let migrated = store.load().unwrap();
    let membership_id = migrated.orchestration_groups[0].workers[0].membership_id;

    assert_eq!(migrated.version, State::CURRENT_VERSION);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("\"membership_id\"")
    );
    assert_eq!(
        store.load().unwrap().orchestration_groups[0].workers[0].membership_id,
        membership_id
    );
}

#[test]
fn future_state_version_fails_closed_without_rewriting_data() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let source = br#"{"version":5,"sessions":[],"orchestration_groups":[]}"#;
    fs::write(&path, source).unwrap();

    let error = StateStore::new(path.clone())
        .load()
        .unwrap_err()
        .to_string();

    assert!(error.contains("unsupported state version 5"), "{error}");
    assert_eq!(fs::read(path).unwrap(), source);
}

#[test]
fn state_v4_orchestration_groups_round_trip_arbitrary_references() {
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
                    membership_id: Default::default(),
                    title: None,
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: false,
                        prompt_agent: false,
                    },
                },
                OrchestrationMember {
                    session_id: interactive,
                    membership_id: Default::default(),
                    title: Some("Interactive worker".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: false,
                        open_interactive: true,
                        prompt_agent: false,
                    },
                },
                OrchestrationMember {
                    session_id: both,
                    membership_id: Default::default(),
                    title: Some("Both capabilities".parse().unwrap()),
                    capabilities: OrchestrationCapabilities {
                        observe_output: true,
                        open_interactive: true,
                        prompt_agent: false,
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
fn state_v4_orchestration_validation_fails_closed() {
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
            membership_id: Default::default(),
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
                prompt_agent: false,
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
            membership_id: Default::default(),
            title: None,
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: false,
                prompt_agent: false,
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
fn state_cardinality_and_string_boundaries_are_enforced_before_persistence() {
    let now = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
    let session = || SessionRecord {
        herdr_agent: None,
        id: SessionId::new(),
        host: "host".into(),
        target: "example.test".into(),
        directory: "/work".into(),
        preset: Some("shell".into()),
        command: Some("true".into()),
        tmux_session_id: None,
        ownership_proof: None,
        status: SessionStatus::Running,
        created_at: now,
        last_used_at: now,
        closed_at: None,
        exit_status: None,
    };
    let boundary = "x".repeat(State::MAX_STRING_BYTES);
    let mut cases: Vec<(&str, State, State)> = Vec::new();

    let session_count = State {
        version: State::CURRENT_VERSION,
        sessions: (0..State::MAX_SESSIONS).map(|_| session()).collect(),
        orchestration_groups: Vec::new(),
    };
    let mut session_count_over = session_count.clone();
    session_count_over.sessions.push(session());
    cases.push(("state sessions", session_count, session_count_over));

    for field in [
        "session host",
        "session target",
        "session directory",
        "session preset",
    ] {
        let mut valid = State {
            version: State::CURRENT_VERSION,
            sessions: vec![session()],
            orchestration_groups: Vec::new(),
        };
        match field {
            "session host" => valid.sessions[0].host = boundary.clone(),
            "session target" => valid.sessions[0].target = boundary.clone(),
            "session directory" => valid.sessions[0].directory = boundary.clone(),
            "session preset" => valid.sessions[0].preset = Some(boundary.clone()),
            _ => unreachable!(),
        }
        let mut invalid = valid.clone();
        match field {
            "session host" => invalid.sessions[0].host.push('x'),
            "session target" => invalid.sessions[0].target.push('x'),
            "session directory" => invalid.sessions[0].directory.push('x'),
            "session preset" => invalid.sessions[0].preset.as_mut().unwrap().push('x'),
            _ => unreachable!(),
        }
        cases.push((field, valid, invalid));
    }

    let mut command = State {
        version: State::CURRENT_VERSION,
        sessions: vec![session()],
        orchestration_groups: Vec::new(),
    };
    command.sessions[0].command = Some("x".repeat(State::MAX_COMMAND_BYTES));
    let mut command_over = command.clone();
    command_over.sessions[0].command.as_mut().unwrap().push('x');
    cases.push(("session command", command, command_over));

    for (category, valid, invalid) in cases {
        valid
            .validate()
            .unwrap_or_else(|error| panic!("{category} boundary rejected: {error}"));
        let error = invalid.validate().unwrap_err().to_string();
        assert!(
            error.contains(category),
            "unexpected {category} error: {error}"
        );

        let temp = tempdir().unwrap();
        let path = temp.path().join("state.json");
        let store = StateStore::new(path.clone());
        store.save(&State::default()).unwrap();
        let before = fs::read(&path).unwrap();
        let save_error = store.save(&invalid).unwrap_err().to_string();
        assert!(
            save_error.contains(category),
            "unexpected persisted {category} error: {save_error}"
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn orchestration_collection_boundaries_succeed_and_n_plus_one_preserves_file() {
    let member = || OrchestrationMember {
        session_id: SessionId::new(),
        membership_id: Default::default(),
        title: None,
        capabilities: OrchestrationCapabilities {
            observe_output: true,
            open_interactive: false,
            prompt_agent: false,
        },
    };
    let group = |index: usize| OrchestrationGroup {
        id: format!("group-{index}").parse().unwrap(),
        title: "Group".parse().unwrap(),
        orchestrator_session_id: SessionId::new(),
        workers: Vec::new(),
    };

    let exact_groups = State {
        version: State::CURRENT_VERSION,
        sessions: Vec::new(),
        orchestration_groups: (0..State::MAX_ORCHESTRATION_GROUPS).map(group).collect(),
    };
    exact_groups.validate().unwrap();
    let mut over_groups = exact_groups.clone();
    over_groups
        .orchestration_groups
        .push(group(State::MAX_ORCHESTRATION_GROUPS));

    let mut exact_members = State {
        version: State::CURRENT_VERSION,
        sessions: Vec::new(),
        orchestration_groups: vec![group(0)],
    };
    exact_members.orchestration_groups[0].workers = (0..OrchestrationGroup::MAX_WORKERS)
        .map(|_| member())
        .collect();
    exact_members.validate().unwrap();
    let mut over_members = exact_members.clone();
    over_members.orchestration_groups[0].workers.push(member());

    for (category, invalid) in [
        ("state orchestration groups", over_groups),
        ("orchestration group workers", over_members),
    ] {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.json");
        let store = StateStore::new(path.clone());
        store.save(&State::default()).unwrap();
        let before = fs::read(&path).unwrap();
        let error = store.save(&invalid).unwrap_err().to_string();
        assert!(
            error.contains(category),
            "unexpected {category} error: {error}"
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn state_serialized_ceiling_round_trips_current_and_migrated_bytes() {
    let state = state_at_serialized_limit();
    for migrated in [false, true] {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.json");
        let store = StateStore::new(path.clone());
        if migrated {
            let mut legacy = serde_json::to_value(&state).unwrap();
            legacy["version"] = 3.into();
            let source = serde_json::to_vec(&legacy).unwrap();
            assert!(source.len() <= StateStore::MAX_INPUT_BYTES);
            fs::write(&path, source).unwrap();
            assert_eq!(store.load().unwrap(), state);
        } else {
            store.save(&state).unwrap();
        }
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), StateStore::MAX_INPUT_BYTES);
        assert_eq!(store.load().unwrap(), state);
        store.save(&state).unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn state_serialized_n_plus_one_is_rejected_without_changing_file() {
    let mut state = state_at_serialized_limit();
    state
        .sessions
        .last_mut()
        .unwrap()
        .command
        .as_mut()
        .unwrap()
        .push('x');
    state.validate().unwrap();
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let before = b"existing state bytes";
    fs::write(&path, before).unwrap();

    let error = StateStore::new(path.clone())
        .save(&state)
        .unwrap_err()
        .to_string();

    assert!(error.contains("serialized state"), "{error}");
    assert!(error.contains("at most"), "{error}");
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn oversized_state_input_is_rejected_before_json_parsing() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    fs::write(&path, vec![b'x'; StateStore::MAX_INPUT_BYTES + 1]).unwrap();

    let error = StateStore::new(path).load().unwrap_err().to_string();
    assert!(error.contains("state input"));
    assert!(error.contains("at most"));
}

#[test]
fn state_v4_rejects_unsafe_orchestration_json() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let store = StateStore::new(path.clone());
    let orchestrator = "tether-0197f198000070008000000000000001";
    let worker = "tether-0197f198000070008000000000000002";
    let invalid_sources = [
        format!(
            r#"{{"version":4,"sessions":[],"orchestration_groups":[{{"id":"Bad ID","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[]}}]}}"#
        ),
        format!(
            r#"{{"version":4,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"line\nbreak","orchestrator_session_id":"{orchestrator}","workers":[]}}]}}"#
        ),
        format!(
            r#"{{"version":4,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[{{"session_id":"{worker}","membership_id":"0197f198000070008000000000000011","title":" trailing ","capabilities":{{"observe_output":true,"open_interactive":false}}}}]}}]}}"#
        ),
        format!(
            r#"{{"version":4,"sessions":[],"orchestration_groups":[{{"id":"valid","title":"Valid","orchestrator_session_id":"{orchestrator}","workers":[{{"session_id":"{worker}","membership_id":"0197f198000070008000000000000011","capabilities":{{"observe_output":true}}}}]}}]}}"#
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
        r#"{"version":4,"sessions":[],"orchestration_groups":[],"unknown":true}"#.to_owned(),
        r#"{"version":4,"sessions":[],"orchestration_groups":[{"id":"valid","title":"Valid","orchestrator_session_id":"tether-0197f198000070008000000000000001","workers":[],"unknown":true}]}"#.to_owned(),
        r#"{"version":4,"sessions":[],"orchestration_groups":[{"id":"valid","title":"Valid","orchestrator_session_id":"tether-0197f198000070008000000000000001","workers":[{"session_id":"tether-0197f198000070008000000000000002","membership_id":"0197f198000070008000000000000011","capabilities":{"observe_output":true,"open_interactive":false,"unknown":true}}]}]}"#.to_owned(),
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

    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 4"));
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
                        herdr_agent: None,
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
fn retargeted_config_symlink_updates_the_locked_target() {
    let temp = tempdir().unwrap();
    let target_a = temp.path().join("config-a.toml");
    let target_b = temp.path().join("config-b.toml");
    let link = temp.path().join("config.toml");
    fs::write(
        &target_a,
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    fs::write(
        &target_b,
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    symlink(&target_a, &link).unwrap();

    ConfigStore::new(link.clone())
        .update(|config| {
            fs::remove_file(&link)?;
            symlink(&target_b, &link)?;
            config.ui.placement = Placement::SplitDown;
            Ok(())
        })
        .unwrap();

    assert_eq!(
        ConfigStore::new(target_a).load().unwrap().ui.placement,
        Placement::SplitDown
    );
    assert_eq!(
        ConfigStore::new(target_b).load().unwrap().ui.placement,
        Placement::SplitRight
    );
}

#[cfg(unix)]
#[test]
fn replaced_locked_target_is_rejected_without_following_the_new_link() {
    let temp = tempdir().unwrap();
    let target = temp.path().join("config-target.toml");
    let victim = temp.path().join("config-victim.toml");
    let link = temp.path().join("config.toml");
    let original = toml::to_string_pretty(&Config::default()).unwrap();
    fs::write(&target, &original).unwrap();
    fs::write(&victim, &original).unwrap();
    symlink(&target, &link).unwrap();

    let error = ConfigStore::new(link)
        .update(|config| {
            fs::remove_file(&target)?;
            symlink(&victim, &target)?;
            config.ui.placement = Placement::SplitDown;
            Ok(())
        })
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("regular file"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fs::read_to_string(victim).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn final_config_symlink_secures_original_parent_and_preserves_target_parent() {
    let temp = tempdir().unwrap();
    let config_dir = temp.path().join("config");
    let target_dir = temp.path().join("stow");
    fs::create_dir(&config_dir).unwrap();
    fs::create_dir(&target_dir).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let target = target_dir.join("config.toml");
    let link = config_dir.join("config.toml");
    fs::write(&target, toml::to_string_pretty(&Config::default()).unwrap()).unwrap();
    symlink(&target, &link).unwrap();

    ConfigStore::new(link).load().unwrap();

    assert_eq!(
        fs::metadata(config_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(target_dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn config_under_symlinked_parent_preserves_target_directory_mode() {
    let temp = tempdir().unwrap();
    let target_dir = temp.path().join("stow");
    let config_dir = temp.path().join("config");
    fs::create_dir(&target_dir).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        target_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    symlink(&target_dir, &config_dir).unwrap();

    ConfigStore::new(config_dir.join("config.toml"))
        .load()
        .unwrap();

    assert_eq!(
        fs::metadata(target_dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn config_under_nested_symlinked_ancestor_preserves_target_directory_mode() {
    let temp = tempdir().unwrap();
    let target_root = temp.path().join("stow");
    let target_dir = target_root.join("tether");
    let linked_root = temp.path().join("linked-stow");
    fs::create_dir_all(&target_dir).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        target_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    symlink(&target_root, &linked_root).unwrap();

    ConfigStore::new(linked_root.join("tether/config.toml"))
        .load()
        .unwrap();

    assert_eq!(
        fs::metadata(target_dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn state_follows_symlink_and_rejects_fifo_storage_paths() {
    let temp = tempdir().unwrap();
    let target = temp.path().join("target.json");
    fs::write(&target, r#"{"version":2,"sessions":[]}"#).unwrap();
    let symlink_path = temp.path().join("symlink-state.json");
    std::os::unix::fs::symlink(&target, &symlink_path).unwrap();

    let state = StateStore::new(symlink_path.clone()).load().unwrap();
    assert!(state.sessions.is_empty());
    assert!(
        fs::symlink_metadata(symlink_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains(r#""version": 4"#)
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
fn config_and_herdr_links_reject_fifo_targets() {
    let temp = tempdir().unwrap();
    let fifo = temp.path().join("config.fifo");
    let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let tether_link = temp.path().join("tether.toml");
    let herdr_link = temp.path().join("herdr.toml");
    symlink(&fifo, &tether_link).unwrap();
    symlink(&fifo, &herdr_link).unwrap();

    for error in [
        ConfigStore::new(tether_link).load().unwrap_err(),
        HerdrKeybindingStore::new(herdr_link).install().unwrap_err(),
    ] {
        let error = format!("{error:#}");
        assert!(error.contains("regular file"), "unexpected error: {error}");
    }
}

#[cfg(unix)]
#[test]
fn config_reader_revalidates_a_fifo_replacement_after_resolution() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, toml::to_string_pretty(&Config::default()).unwrap()).unwrap();
    let lock_path = temp.path().join(".config.toml.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let load_path = path.clone();
    let loader = thread::spawn(move || {
        done_tx.send(ConfigStore::new(load_path).load()).unwrap();
    });

    for _ in 0..100 {
        let parent = fs::File::open(temp.path()).unwrap();
        match parent.try_lock_exclusive() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Ok(()) => FileExt::unlock(&parent).unwrap(),
            Err(error) => panic!("unexpected parent-lock error: {error}"),
        }
        thread::sleep(StdDuration::from_millis(10));
    }
    let parent = fs::File::open(temp.path()).unwrap();
    assert_eq!(
        parent.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    fs::remove_file(&path).unwrap();
    let fifo_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    FileExt::unlock(&lock).unwrap();

    let error = done_rx
        .recv_timeout(StdDuration::from_secs(2))
        .expect("config reader blocked on the replacement FIFO")
        .unwrap_err();
    loader.join().unwrap();
    assert!(
        format!("{error:#}").contains("regular file"),
        "unexpected error: {error:#}"
    );
}

#[cfg(unix)]
#[test]
fn state_reader_rejects_a_symlink_replacement_after_resolution() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.json");
    let victim = temp.path().join("victim.json");
    StateStore::new(path.clone())
        .save(&State::default())
        .unwrap();
    StateStore::new(victim.clone())
        .save(&State::default())
        .unwrap();
    let original_victim = fs::read(&victim).unwrap();
    let lock_path = temp.path().join(".state.json.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let load_path = path.clone();
    let loader = thread::spawn(move || {
        done_tx.send(StateStore::new(load_path).load()).unwrap();
    });

    for _ in 0..100 {
        let parent = fs::File::open(temp.path()).unwrap();
        match parent.try_lock_exclusive() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Ok(()) => FileExt::unlock(&parent).unwrap(),
            Err(error) => panic!("unexpected parent-lock error: {error}"),
        }
        thread::sleep(StdDuration::from_millis(10));
    }
    let parent = fs::File::open(temp.path()).unwrap();
    assert_eq!(
        parent.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    fs::remove_file(&path).unwrap();
    symlink(&victim, &path).unwrap();
    FileExt::unlock(&lock).unwrap();

    done_rx
        .recv_timeout(StdDuration::from_secs(2))
        .expect("state reader blocked after the replacement")
        .unwrap_err();
    loader.join().unwrap();
    assert_eq!(fs::read(victim).unwrap(), original_victim);
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

#[cfg(unix)]
#[test]
fn herdr_keybinding_install_and_rollback_follow_stowed_config() {
    let temp = tempdir().unwrap();
    let config_dir = temp.path().join("herdr");
    let stow_dir = temp.path().join("stow");
    fs::create_dir(&config_dir).unwrap();
    fs::create_dir(&stow_dir).unwrap();
    let stow_mode = fs::metadata(&stow_dir).unwrap().permissions().mode() & 0o777;
    let target = stow_dir.join("config.toml");
    let link = config_dir.join("config.toml");
    let original = b"# stowed\nonboarding = false\n";
    fs::write(&target, original).unwrap();
    symlink("../stow/config.toml", &link).unwrap();
    let store = HerdrKeybindingStore::new(link.clone());

    let installed = store.install().unwrap();
    let backup = match installed {
        HerdrKeybindingInstall::Installed { backup } => backup,
        other => panic!("unexpected install result: {other:?}"),
    };
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert!(
        fs::read(&target)
            .unwrap()
            .ends_with(TETHER_BINDING.as_bytes())
    );
    assert!(backup.is_file());

    store.rollback().unwrap();

    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(fs::read(&target).unwrap(), original);
    assert!(!backup.exists());
    assert_eq!(
        fs::metadata(&stow_dir).unwrap().permissions().mode() & 0o777,
        stow_mode
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

#[cfg(unix)]
#[test]
fn herdr_keybinding_rejects_backup_symlink_without_touching_target() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = b"onboarding = false\n";
    fs::write(&path, original).unwrap();
    let backup = HerdrKeybindingStore::backup_path_for(&path);
    let victim = temp.path().join("victim.toml");
    fs::write(&victim, original).unwrap();
    symlink(&victim, &backup).unwrap();

    let error = HerdrKeybindingStore::new(path.clone())
        .install()
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("backup"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read(victim).unwrap(), original);
    assert!(fs::symlink_metadata(backup).unwrap().is_symlink());
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

#[test]
fn prompt_agent_capability_is_additive_and_defaults_closed() {
    let legacy: OrchestrationCapabilities =
        serde_json::from_str(r#"{"observe_output":true,"open_interactive":true}"#).unwrap();
    assert!(!legacy.prompt_agent);

    let granted = OrchestrationCapabilities {
        observe_output: true,
        open_interactive: true,
        prompt_agent: true,
    };
    let granted_json = serde_json::to_value(granted).unwrap();
    assert_eq!(granted_json["prompt_agent"], true);

    let default_json = serde_json::to_value(legacy).unwrap();
    assert!(default_json.get("prompt_agent").is_none());
}
