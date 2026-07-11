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
    model::Placement,
    state::{SessionRecord, SessionStatus, State, StateStore},
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
            status: SessionStatus::Active,
            created_at: now,
            last_used_at: now,
            closed_at: None,
        }],
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
    assert_eq!(migrated.sessions[0].status, SessionStatus::Active);
    assert!(migrated.sessions[0].preset.is_none());
    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 1"));
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

    assert!(fs::read_to_string(path).unwrap().contains("\"version\": 1"));
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
                        status: SessionStatus::Running,
                        created_at: now,
                        last_used_at: now,
                        closed_at: None,
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
