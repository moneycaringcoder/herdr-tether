use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
    time::Duration as StdDuration,
};

use chrono::{TimeZone, Utc};
use herdr_tether::{
    config::{CommandPreset, Config, ConfigStore, HostConfig, UiDefaults},
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
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "atomic write must not leave temporary files"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn config_v0_is_migrated_and_rewritten_as_v1() {
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

    assert_eq!(migrated.version, Config::CURRENT_VERSION);
    assert_eq!(migrated.hosts[0].name, "work");
    assert!(migrated.hosts[0].presets.is_empty());
    assert_eq!(migrated.ui.placement, Placement::SplitRight);
    assert!(fs::read_to_string(path).unwrap().contains("version = 1"));
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
                        status: SessionStatus::Active,
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
