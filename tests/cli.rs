use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use herdr_tether::{config::HerdrKeybindingStore, model::SessionId as GeneratedSessionId};
use predicates::prelude::*;
use tempfile::TempDir;

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    thread::JoinHandle,
};

static CLI_TEST_LOCK: Mutex<()> = Mutex::new(());

struct Sandbox {
    temp: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().unwrap(),
            _guard: CLI_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.temp.path().join(name)
    }

    fn command(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_herdr-tether"));
        command
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_STATE_HOME", self.path("xdg-state"))
            .env_remove("HERDR_PLUGIN_CONFIG_DIR")
            .env_remove("HERDR_PLUGIN_STATE_DIR")
            .env_remove("HERDR_PLUGIN_ACTION_ID")
            .env_remove("HERDR_PLUGIN_ENTRYPOINT_ID")
            .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("PANE_ID")
            .env_remove("WORKSPACE_ID");
        command
    }

    fn config_file(&self) -> std::path::PathBuf {
        self.path("xdg-config/herdr-tether/config.toml")
    }

    fn state_file(&self) -> std::path::PathBuf {
        self.path("xdg-state/herdr-tether/state.json")
    }
}

#[cfg(unix)]
fn prepare_doctor_sandbox(sandbox: &Sandbox) -> std::path::PathBuf {
    let (bin, herdr) = install_setup_runtime_scripts(sandbox, None);
    sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_BIN_PATH", herdr)
        .args(["setup", "--yes"])
        .assert()
        .success();
    let cargo = bin.join("cargo");
    fs::write(&cargo, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(cargo, fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

#[cfg(unix)]
fn serve_snapshot(
    sandbox: &Sandbox,
    name: &str,
    protocol: u32,
) -> (std::path::PathBuf, JoinHandle<()>) {
    let socket = sandbox.path(name);
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "session.snapshot");
        assert_eq!(request["params"], serde_json::json!({}));
        let id = request["id"].as_str().unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": id,
                "result": {
                    "type": "session_snapshot",
                    "snapshot": {
                        "version": "0.8.0",
                        "protocol": protocol,
                        "panes": [],
                        "agents": [],
                    }
                }
            })
        )
        .unwrap();
        stream.flush().unwrap();
    });
    (socket, server)
}

const SESSION_ID: &str = "tether-0197f198000070008000000000000001";

fn active_state(id: &str) -> String {
    active_state_many(&[id.to_owned()])
}

fn active_state_many(ids: &[String]) -> String {
    let sessions = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            serde_json::json!({
                "id": id,
                "host": "local",
                "target": "local",
                "directory": "/tmp",
                "preset": null,
                "command": "exec true",
                "tmux_session_id": index + 7,
                "ownership_proof": format!(
                    "0197f1980000700080000000000000{:02x}",
                    0x99 + index
                ),
                "status": "running",
                "created_at": "2026-01-01T00:00:00Z",
                "last_used_at": "2026-01-01T00:00:00Z",
                "closed_at": null
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 4,
        "sessions": sessions,
        "orchestration_groups": []
    }))
    .unwrap()
}

fn seed_active_state(sandbox: &Sandbox, ids: &[String]) {
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), active_state_many(ids)).unwrap();
}

fn install_tmux_script(sandbox: &Sandbox, body: &str) -> (std::ffi::OsString, std::path::PathBuf) {
    let bin = sandbox.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    fs::write(&tmux, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original_path)))
            .unwrap();
    (path, tmux)
}

fn install_setup_runtime_scripts(
    sandbox: &Sandbox,
    failing_tool: Option<&str>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let bin = sandbox.path("setup-bin");
    fs::create_dir_all(&bin).unwrap();
    for (tool, expected_arg) in [("herdr", "--version"), ("tmux", "-V"), ("ssh", "-V")] {
        let executable = bin.join(tool);
        let exit = if failing_tool == Some(tool) { 19 } else { 0 };
        fs::write(
            &executable,
            format!("#!/bin/sh\n[ \"$1\" = \"{expected_arg}\" ] || exit 31\nexit {exit}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    (bin.clone(), bin.join("herdr"))
}

#[test]
fn setup_preflight_checks_each_runtime_without_mutating_absent_or_existing_stores() {
    for (tool, display_name) in [("herdr", "Herdr"), ("tmux", "tmux"), ("ssh", "SSH")] {
        for existing in [false, true] {
            let sandbox = Sandbox::new();
            let (bin, herdr) = install_setup_runtime_scripts(&sandbox, Some(tool));
            let config_dir = sandbox.path("preflight-config");
            let state_dir = sandbox.path("preflight-state");

            if existing {
                fs::create_dir_all(&config_dir).unwrap();
                fs::create_dir_all(&state_dir).unwrap();
                fs::write(config_dir.join("config.toml"), b"exact config bytes\r\n").unwrap();
                fs::write(state_dir.join("state.json"), b"exact state bytes\r\n").unwrap();
            }

            sandbox
                .command()
                .env("PATH", &bin)
                .env("HERDR_BIN_PATH", &herdr)
                .env("HERDR_PLUGIN_CONFIG_DIR", &config_dir)
                .env("HERDR_PLUGIN_STATE_DIR", &state_dir)
                .args(["setup", "--yes"])
                .assert()
                .failure()
                .stderr(predicate::str::contains(format!(
                    "setup prerequisite unavailable: {display_name}"
                )))
                .stderr(predicate::str::contains("exit").not())
                .stderr(predicate::str::contains("setup-bin").not());

            if existing {
                assert_eq!(
                    fs::read(config_dir.join("config.toml")).unwrap(),
                    b"exact config bytes\r\n"
                );
                assert_eq!(
                    fs::read(state_dir.join("state.json")).unwrap(),
                    b"exact state bytes\r\n"
                );
            } else {
                assert!(!config_dir.exists());
                assert!(!state_dir.exists());
            }
        }
    }
}

#[test]
fn setup_preflight_success_creates_stores_without_requiring_cargo() {
    let sandbox = Sandbox::new();
    let (bin, herdr) = install_setup_runtime_scripts(&sandbox, None);

    sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_BIN_PATH", herdr)
        .args(["setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Herdr, tmux, and SSH"))
        .stdout(predicate::str::contains("Cargo").not());

    assert!(sandbox.config_file().exists());
    assert!(sandbox.state_file().exists());
}

#[cfg(unix)]
#[test]
fn setup_follows_stowed_config_and_state_files() {
    use std::os::unix::fs::symlink;

    let sandbox = Sandbox::new();
    let (bin, herdr) = install_setup_runtime_scripts(&sandbox, None);
    let run_setup = || {
        sandbox
            .command()
            .env("PATH", &bin)
            .env("HERDR_BIN_PATH", &herdr)
            .args(["setup", "--yes"])
            .assert()
            .success();
    };
    run_setup();

    let stow = sandbox.path("stow");
    fs::create_dir(&stow).unwrap();
    let stow_mode = fs::metadata(&stow).unwrap().permissions().mode() & 0o777;
    let config_target = stow.join("config.toml");
    let state_target = stow.join("state.json");
    fs::rename(sandbox.config_file(), &config_target).unwrap();
    fs::rename(sandbox.state_file(), &state_target).unwrap();
    symlink(Path::new("../../stow/config.toml"), sandbox.config_file()).unwrap();
    symlink(Path::new("../../stow/state.json"), sandbox.state_file()).unwrap();

    run_setup();

    assert!(
        fs::symlink_metadata(sandbox.config_file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(sandbox.state_file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(config_target.is_file());
    assert!(state_target.is_file());
    assert_eq!(
        fs::metadata(&stow).unwrap().permissions().mode() & 0o777,
        stow_mode
    );
}

#[test]
fn keybinding_preflight_failure_preserves_exact_herdr_config() {
    let sandbox = Sandbox::new();
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    let original = b"# exact private bytes\r\nonboarding = false\r\n";
    fs::write(&herdr_config, original).unwrap();
    let (bin, herdr) = install_setup_runtime_scripts(&sandbox, Some("herdr"));

    sandbox
        .command()
        .env("PATH", bin)
        .env("HERDR_BIN_PATH", herdr)
        .args(["setup", "keybinding"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "setup prerequisite unavailable: Herdr",
        ));

    assert_eq!(fs::read(&herdr_config).unwrap(), original);
    assert!(!HerdrKeybindingStore::backup_path_for(&herdr_config).exists());
}

#[test]
fn plugin_error_boundary_redacts_details_and_validates_correlation_reference() {
    let sandbox = Sandbox::new();
    let secret_dir = sandbox.path("SECRET-target-path-backend");
    fs::create_dir_all(&secret_dir).unwrap();
    fs::write(secret_dir.join("config.toml"), "SECRET_BACKEND = [").unwrap();

    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &secret_dir)
        .env("HERDR_PLUGIN_ACTION_ID", "SECRET_ACTION")
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"correlation_id":"safe.Ref:42-test_ok"}"#,
        )
        .args(["host", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "plugin command failed; correlation safe.Ref:42-test_ok",
        ))
        .stderr(predicate::str::contains("SECRET").not())
        .stderr(predicate::str::contains(secret_dir.display().to_string()).not());

    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &secret_dir)
        .env("HERDR_PLUGIN_ENTRYPOINT_ID", "SECRET_ENTRYPOINT")
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"correlation_id":"hostile/path\nSECRET"}"#,
        )
        .args(["host", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "plugin command failed; correlation unavailable",
        ))
        .stderr(predicate::str::contains("SECRET").not());

    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &secret_dir)
        .args(["host", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(secret_dir.display().to_string()))
        .stderr(predicate::str::contains("SECRET_BACKEND"));
}

#[test]
fn every_scriptable_surface_has_help() {
    let sandbox = Sandbox::new();
    let commands: &[&[&str]] = &[
        &["--help"],
        &["setup", "--help"],
        &["setup", "keybinding", "--help"],
        &["host", "--help"],
        &["host", "add", "--help"],
        &["host", "list", "--help"],
        &["host", "remove", "--help"],
        &["host", "check", "--help"],
        &["open", "--help"],
        &["snapshot", "--help"],
        &["session", "--help"],
        &["session", "list", "--help"],
        &["session", "open", "--help"],
        &["session", "restart", "--help"],
        &["session", "stop", "--help"],
        &["session", "remove", "--help"],
        &["session", "prune", "--help"],
        &["doctor", "--help"],
        &["orchestration", "--help"],
        &["orchestration", "create", "--help"],
        &["orchestration", "delete", "--help"],
        &["orchestration", "list", "--help"],
        &["orchestration", "add-worker", "--help"],
        &["orchestration", "remove-worker", "--help"],
        &["orchestration", "observe", "--help"],
    ];

    for arguments in commands {
        sandbox
            .command()
            .args(*arguments)
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}

#[test]
fn orchestration_crud_is_opt_in_state_only_and_exact_id() {
    let sandbox = Sandbox::new();
    let worker = "tether-0197f198000070008000000000000002";
    seed_active_state(&sandbox, &[SESSION_ID.to_owned(), worker.to_owned()]);

    sandbox
        .command()
        .args([
            "orchestration",
            "create",
            "build-fleet",
            "--title",
            "Build fleet",
            "--orchestrator",
            SESSION_ID,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("created build-fleet"));
    sandbox
        .command()
        .args([
            "orchestration",
            "add-worker",
            "build-fleet",
            worker,
            "--title",
            "Compiler",
            "--observe-output",
            "--open-interactive",
        ])
        .assert()
        .success();
    sandbox
        .command()
        .args(["orchestration", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"id\": \"build-fleet\""))
        .stdout(predicate::str::contains("\"session_id\""))
        .stdout(predicate::str::contains("\"membership_id\""))
        .stdout(predicate::str::contains("\"observe_output\": true"))
        .stdout(predicate::str::contains("\"open_interactive\": true"));

    sandbox
        .command()
        .args(["orchestration", "remove-worker", "build-fleet", SESSION_ID])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "worker `{SESSION_ID}` is not a member"
        )));
    sandbox
        .command()
        .args(["orchestration", "add-worker", "build-fleet", SESSION_ID])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "must declare at least one capability",
        ));
    sandbox
        .command()
        .args(["orchestration", "remove-worker", "build-fleet", worker])
        .assert()
        .success();
    sandbox
        .command()
        .args(["orchestration", "delete", "build-fleet"])
        .assert()
        .success();
    sandbox
        .command()
        .args(["orchestration", "delete", "build-fleet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unknown orchestration group `build-fleet`",
        ));

    let state = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(state.contains("\"version\": 4"));
    assert!(state.contains("\"orchestration_groups\": []"));
}

#[test]
fn orchestration_group_and_worker_limits_are_enforced_without_pane_side_effects() {
    let sandbox = Sandbox::new();
    let workers = (0..65)
        .map(|_| GeneratedSessionId::new().to_string())
        .collect::<Vec<_>>();
    let sessions = std::iter::once(SESSION_ID.to_owned())
        .chain(workers.iter().cloned())
        .collect::<Vec<_>>();
    seed_active_state(&sandbox, &sessions);
    for index in 0..32 {
        sandbox
            .command()
            .args([
                "orchestration",
                "create",
                &format!("group-{index}"),
                "--title",
                &format!("Group {index}"),
                "--orchestrator",
                SESSION_ID,
            ])
            .assert()
            .success();
    }
    sandbox
        .command()
        .args([
            "orchestration",
            "create",
            "group-overflow",
            "--title",
            "Overflow",
            "--orchestrator",
            SESSION_ID,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "orchestration group limit of 32 has been reached",
        ));

    for worker in workers.iter().take(64) {
        sandbox
            .command()
            .args([
                "orchestration",
                "add-worker",
                "group-0",
                worker,
                "--observe-output",
            ])
            .assert()
            .success();
    }
    sandbox
        .command()
        .args([
            "orchestration",
            "add-worker",
            "group-0",
            &workers[64],
            "--observe-output",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "orchestration worker limit of 64 has been reached",
        ));
}
#[test]
fn help_uses_the_same_lifecycle_vocabulary_as_the_picker() {
    let sandbox = Sandbox::new();

    sandbox
        .command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Open the Tether picker, or create and open a workload with options",
        ))
        .stdout(predicate::str::contains(
            "Inspect and manage Tether-owned workloads",
        ));

    sandbox
        .command()
        .args(["session", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Open an existing running workload",
        ))
        .stdout(predicate::str::contains("Restart an ended workload"))
        .stdout(predicate::str::contains(
            "Stop the exact Tether-owned workload",
        ))
        .stdout(predicate::str::contains("Remove an ended workload"));
}

#[test]
fn host_commands_persist_explicit_hosts_and_surface_ssh_aliases() {
    let sandbox = Sandbox::new();
    let ssh_dir = sandbox.path("home/.ssh");
    fs::create_dir_all(&ssh_dir).unwrap();
    fs::write(
        ssh_dir.join("config"),
        "Host from-ssh-config\n  HostName 192.0.2.20\n",
    )
    .unwrap();

    sandbox
        .command()
        .args([
            "host",
            "add",
            "build-box",
            "ssh://builder@example.test:2222",
            "--root",
            "/srv/code",
            "--preset",
            "shell=exec ${SHELL:-/bin/sh} -l",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("build-box"));

    sandbox
        .command()
        .args(["host", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("build-box"))
        .stdout(predicate::str::contains("from-ssh-config"))
        .stdout(predicate::str::contains("configured"))
        .stdout(predicate::str::contains("ssh_config"));

    assert!(sandbox.config_file().exists());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(sandbox.config_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    sandbox
        .command()
        .args(["host", "remove", "build-box"])
        .assert()
        .success();
    sandbox
        .command()
        .args(["host", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("build-box").not());
}

#[test]
fn local_host_check_resolves_tmux_outside_restricted_gui_path() {
    let sandbox = Sandbox::new();
    let fallback = [
        "/usr/bin/tmux",
        "/bin/tmux",
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
    ]
    .into_iter()
    .find(|candidate| Path::new(candidate).is_file())
    .expect("compatibility tests require tmux in a supported fallback location");

    sandbox
        .command()
        .env("PATH", sandbox.path("restricted-gui-path"))
        .args(["host", "check", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tmux "));

    assert!(Path::new(fallback).is_absolute());
}

#[test]
fn plugin_directories_override_xdg_and_setup_never_edits_herdr_config() {
    let sandbox = Sandbox::new();
    let (runtime_bin, herdr) = install_setup_runtime_scripts(&sandbox, None);
    let plugin_config = sandbox.path("plugin-config");
    let plugin_state = sandbox.path("plugin-state");
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    fs::write(&herdr_config, "# sentinel\n").unwrap();

    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &plugin_config)
        .env("HERDR_PLUGIN_STATE_DIR", &plugin_state)
        .env("PATH", &runtime_bin)
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plugin_action"))
        .stdout(predicate::str::contains("moneycaringcoder.tether.open"));

    let first_config = fs::read(plugin_config.join("config.toml")).unwrap();
    let first_state = fs::read(plugin_state.join("state.json")).unwrap();
    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &plugin_config)
        .env("HERDR_PLUGIN_STATE_DIR", &plugin_state)
        .env("PATH", &runtime_bin)
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Next: install prefix+t with `herdr-tether setup keybinding`",
        ));
    assert_eq!(
        fs::read(plugin_config.join("config.toml")).unwrap(),
        first_config
    );
    assert_eq!(
        fs::read(plugin_state.join("state.json")).unwrap(),
        first_state
    );

    assert!(plugin_config.join("config.toml").exists());
    assert!(plugin_state.join("state.json").exists());
    assert_eq!(fs::read_to_string(herdr_config).unwrap(), "# sentinel\n");
    assert!(!sandbox.config_file().exists());
}

#[test]
fn keybinding_setup_and_rollback_are_explicit_and_reload_after_writes() {
    let sandbox = Sandbox::new();
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    let original = b"# preserve\r\nonboarding = false\r\n";
    fs::write(&herdr_config, original).unwrap();
    let bin = sandbox.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    let log = sandbox.path("reload.log");
    fs::write(
        &herdr,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nprintf '%s %s\\n' \"$1\" \"$2\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();

    sandbox
        .command()
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "keybinding"])
        .assert()
        .success()
        .stdout(predicate::str::contains("prefix+t"))
        .stdout(predicate::str::contains("moneycaringcoder.tether.open"))
        .stdout(predicate::str::contains(
            "Next: press prefix+t in Herdr to open Tether",
        ));
    assert!(
        fs::read_to_string(&herdr_config)
            .unwrap()
            .contains("moneycaringcoder.tether.open")
    );
    assert_eq!(fs::read_to_string(&log).unwrap(), "server reload-config\n");

    sandbox
        .command()
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "keybinding"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already bound"));

    sandbox
        .command()
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "keybinding", "--rollback"])
        .assert()
        .success()
        .stdout(predicate::str::contains("restored"));
    assert_eq!(fs::read(&herdr_config).unwrap(), original);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "server reload-config\nserver reload-config\nserver reload-config\n"
    );
}

#[test]
fn keybinding_reload_failure_reports_written_state_and_rollback_without_leaking_config() {
    let sandbox = Sandbox::new();
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    fs::write(
        &herdr_config,
        "# private-token = \"do-not-print\"\nonboarding = false\n",
    )
    .unwrap();
    let herdr = sandbox.path("herdr-fails");
    fs::write(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nexit 23\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();

    sandbox
        .command()
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "keybinding"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reload failed with status"))
        .stderr(predicate::str::contains("setup keybinding --rollback"))
        .stderr(predicate::str::contains("do-not-print").not());

    assert!(
        fs::read_to_string(&herdr_config)
            .unwrap()
            .contains("moneycaringcoder.tether.open")
    );
    assert!(HerdrKeybindingStore::backup_path_for(&herdr_config).exists());

    sandbox
        .command()
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "keybinding", "--rollback"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "rerun `herdr server reload-config`",
        ))
        .stderr(predicate::str::contains("setup keybinding --rollback").not());
    assert_eq!(
        fs::read_to_string(&herdr_config).unwrap(),
        "# private-token = \"do-not-print\"\nonboarding = false\n"
    );
    assert!(!HerdrKeybindingStore::backup_path_for(&herdr_config).exists());
}

fn write_fake_tmux(path: &Path, log: &Path) {
    let proof_file = log.with_extension("proof");
    let id_file = log.with_extension("id");
    let script = format!(
        r#"#!/bin/sh
printf '%s' "$1" >> '{log}'
command=$1
shift
for arg do printf ' <%s>' "$arg" >> '{log}'; done
printf '\n' >> '{log}'
case "$command" in
  new-session)
    previous=
    for arg do
      if [ "$previous" = '-s' ]; then printf '%s' "$arg" > '{id_file}'; fi
      case "$arg" in TETHER_OWNERSHIP_PROOF=*) printf '%s' "${{arg#*=}}" > '{proof_file}' ;; esac
      previous=$arg
    done
    printf '$7:%%3'
    ;;
  list-sessions)
    id=$(cat '{id_file}' 2>/dev/null)
    proof=$(cat '{proof_file}' 2>/dev/null)
    case "$*" in
      *TETHER_OWNERSHIP_PROOF*) if [ -n "$id" ]; then printf '%s:$7:0:0::%s' "$id" "$proof"; fi ;;
      *'#{{session_id}}'*) if [ -n "$id" ]; then printf '%s:$7' "$id"; fi ;;
      *) if [ -n "$id" ]; then printf '%s:0:1:1:0::' "$id"; fi ;;
    esac
    ;;
  if-shell)
    case "$*" in *'kill-session -t'*) rm -f '{id_file}' '{proof_file}' ;; esac
    ;;
  display-message) printf '%s' '/tmp/project with spaces' ;;
esac
exit 0
"#,
        log = log.display(),
        proof_file = proof_file.display(),
        id_file = id_file.display(),
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn local_open_resume_close_and_prune_preserve_lifecycle_contracts() {
    let sandbox = Sandbox::new();
    let bin = sandbox.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    let log = sandbox.path("tmux.log");
    write_fake_tmux(&tmux, &log);
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();

    let output = sandbox
        .command()
        .env("PATH", &path)
        .args([
            "open",
            "--host",
            "local",
            "--directory",
            "/tmp/project with spaces",
            "--command",
            "printf 'ready\\n'",
            "--placement",
            "split-right",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let created = String::from_utf8(output.stdout).unwrap();
    let session_id = created
        .split_whitespace()
        .find(|word| word.starts_with("tether-"))
        .expect("open prints the stable session id")
        .trim_end_matches(|character: char| !character.is_ascii_hexdigit() && character != '-')
        .to_owned();

    let transcript = fs::read_to_string(&log).unwrap();
    assert!(transcript.contains("new-session"));
    assert!(transcript.contains("attach-session"));
    assert!(!transcript.contains("kill-session"));

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&session_id))
        .stdout(predicate::str::contains("/tmp/project with spaces"));

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "open", &session_id])
        .assert()
        .success();
    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "stop", &session_id])
        .assert()
        .success();
    assert!(fs::read_to_string(&log).unwrap().contains("kill-session"));

    let before = fs::read(sandbox.state_file()).unwrap();
    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "prune", "--dry-run", "--older-than-days", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&session_id));
    assert_eq!(fs::read(sandbox.state_file()).unwrap(), before);

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "prune", "--older-than-days", "0"])
        .assert()
        .success()
        .stdout(format!("{session_id}\n"));
    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&session_id).not());
}

#[test]
fn unknown_hosts_fail_without_mutating_state() {
    let sandbox = Sandbox::new();
    sandbox
        .command()
        .args([
            "open",
            "--host",
            "missing",
            "--directory",
            "/tmp",
            "--command",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown host"));
    assert!(!sandbox.state_file().exists());
}

#[test]
fn host_check_reports_remote_tmux_and_read_only_herdr_discovery() {
    let sandbox = Sandbox::new();
    let bin = sandbox.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    let log = sandbox.path("ssh.log");
    let script = format!(
        "#!/bin/sh\nfor arg do printf '<%s>' \"$arg\" >> '{log}'; done\nprintf '\\n' >> '{log}'\ncase \"$*\" in\n  *\"tmux\"*\"-V\"*) printf 'tmux 3.4\\n' ;;\n  *\"herdr\"*\"--version\"*) printf 'herdr 0.8.0\\n' ;;\nesac\nexit 0\n",
        log = log.display()
    );
    fs::write(&ssh, script).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();

    sandbox
        .command()
        .args(["host", "add", "build-box", "builder@example.test"])
        .assert()
        .success();
    sandbox
        .command()
        .env("PATH", &path)
        .args(["host", "check", "build-box"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tmux 3.4"))
        .stdout(predicate::str::contains("herdr 0.8.0"));

    let transcript = fs::read_to_string(log).unwrap();
    assert!(
        transcript
            .lines()
            .all(|line| line.contains("<BatchMode=yes>"))
    );
    assert!(!transcript.contains("StrictHostKeyChecking"));
    assert!(!transcript.contains("herdr --remote"));
}

#[test]
fn stop_marks_a_missing_workload_ended_without_killing_it() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), active_state(SESSION_ID)).unwrap();
    let log = sandbox.path("tmux.log");
    let body = format!(
        "printf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n  list-sessions) exit 0 ;;\n  kill-session) exit 99 ;;\nesac\nexit 0",
        log.display()
    );
    let (path, _) = install_tmux_script(&sandbox, &body);

    sandbox
        .command()
        .env("PATH", path)
        .args(["session", "stop", SESSION_ID])
        .assert()
        .success()
        // Nothing was stopped: the record was already gone, and the command says
        // so rather than claiming an act it did not perform.
        .stdout(format!(
            "no workload found for {SESSION_ID}; recorded as ended\n"
        ));

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("list-sessions"));
    assert!(!transcript.contains("kill-session"));
    let persisted = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(persisted.contains(r#""status": "ended""#));
    assert!(!persisted.contains(r#""closed_at": null"#));
}

#[test]
fn stop_unknown_preserves_running_and_failed_stop_is_recoverable() {
    for (name, body) in [
        (
            "unknown",
            "case \"$1\" in\n  list-sessions) printf 'malformed'; exit 0 ;;\n  kill-session) exit 99 ;;\nesac\nexit 0",
        ),
        (
            "failed-running-close",
            "case \"$1\" in\n  list-sessions) printf 'tether-0197f198000070008000000000000001:$7:0:0::0197f198000070008000000000000099'; exit 0 ;;\n  if-shell) printf 'still running' >&2; exit 2 ;;\nesac\nexit 0",
        ),
    ] {
        let sandbox = Sandbox::new();
        fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
        fs::write(sandbox.state_file(), active_state(SESSION_ID)).unwrap();
        let before = fs::read(sandbox.state_file()).unwrap();
        let log = sandbox.path("tmux.log");
        let instrumented = format!("printf '%s\\n' \"$1\" >> '{}'\n{body}", log.display());
        let (path, _) = install_tmux_script(&sandbox, &instrumented);

        sandbox
            .command()
            .env("PATH", path)
            .args(["session", "stop", SESSION_ID])
            .assert()
            .failure();

        let after = fs::read(sandbox.state_file()).unwrap();
        if name == "unknown" {
            assert_eq!(
                after, before,
                "indeterminate inspect must not rewrite state"
            );
        } else {
            let persisted = String::from_utf8(after).unwrap();
            assert!(
                persisted.contains(r#""status": "stopping""#),
                "failed stop must persist a recoverable stopping marker"
            );
            let document: serde_json::Value = serde_json::from_str(&persisted).unwrap();
            assert!(document["sessions"][0]["closed_at"].is_null());
        }
        let transcript = fs::read_to_string(log).unwrap();
        if name == "unknown" {
            assert!(!transcript.contains("if-shell"));
        } else {
            assert!(transcript.contains("if-shell"));
        }
    }
}

#[test]
fn open_rejects_missing_unknown_and_ended_sessions_without_mutation() {
    for (name, state, body, expected) in [
        (
            "missing",
            active_state(SESSION_ID),
            "case \"$1\" in list-sessions) exit 0;; esac\nexit 0",
            "cannot be opened",
        ),
        (
            "unknown",
            active_state(SESSION_ID),
            "case \"$1\" in list-sessions) printf 'malformed'; exit 0;; esac\nexit 0",
            "could not determine",
        ),
        (
            "closing",
            active_state(SESSION_ID).replace(r#""status": "running""#, r#""status": "stopping""#),
            "exit 99",
            "is stopping",
        ),
        (
            "closed",
            active_state(SESSION_ID)
                .replace(r#""status": "running""#, r#""status": "ended""#)
                .replace(
                    r#""closed_at": null"#,
                    r#""closed_at": "2026-01-01T00:00:01Z""#,
                ),
            "exit 99",
            "has ended; run `herdr-tether session restart",
        ),
    ] {
        let sandbox = Sandbox::new();
        fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
        fs::write(sandbox.state_file(), state).unwrap();
        let before = fs::read(sandbox.state_file()).unwrap();
        let (path, _) = install_tmux_script(&sandbox, body);

        sandbox
            .command()
            .env("PATH", path)
            .args(["session", "open", SESSION_ID])
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));

        assert_eq!(
            fs::read(sandbox.state_file()).unwrap(),
            before,
            "{name} must not mutate metadata"
        );
    }
}

#[test]
fn ended_sessions_have_obvious_restart_stop_and_remove_actions() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    let ended = active_state(SESSION_ID)
        .replace(
            r#""directory": "/tmp""#,
            r#""directory": "/tmp/project with spaces""#,
        )
        .replace(r#""status": "running""#, r#""status": "ended""#)
        .replace(
            r#""closed_at": null"#,
            r#""closed_at": "2026-01-01T00:00:01Z""#,
        );
    fs::write(sandbox.state_file(), ended).unwrap();
    let bin = sandbox.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    let log = sandbox.path("tmux.log");
    write_fake_tmux(&tmux, &log);
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original_path)))
            .unwrap();

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "restart", SESSION_ID])
        .assert()
        .success();
    let restarted = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(restarted.contains(r#""status": "running""#));
    assert!(fs::read_to_string(&log).unwrap().contains("new-session"));

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "stop", SESSION_ID])
        .assert()
        .success()
        .stdout(format!("stopped {SESSION_ID}\n"));
    let stopped = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(stopped.contains(r#""status": "ended""#));

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "remove", SESSION_ID])
        .assert()
        .success()
        .stdout(format!("removed {SESSION_ID}\n"));
    let removed = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(removed.contains(r#""status": "removed""#));
}

#[test]
fn migrated_v022_record_can_only_remove_metadata_without_transport() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(
        sandbox.state_file(),
        format!(
            r#"{{
  "version": 1,
  "sessions": [{{
    "id": "{SESSION_ID}",
    "host": "local",
    "target": "local",
    "directory": "/tmp",
    "preset": null,
    "status": "active",
    "created_at": "2026-01-01T00:00:00Z",
    "last_used_at": "2026-01-01T00:00:00Z",
    "closed_at": null
  }}]
}}"#
        ),
    )
    .unwrap();
    let log = sandbox.path("tmux.log");
    let body = format!("printf invoked > '{}'\nexit 99", log.display());
    let (path, _) = install_tmux_script(&sandbox, &body);

    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "stop", SESSION_ID])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no private ownership proof"));
    assert!(!log.exists(), "legacy Stop must not invoke tmux");

    sandbox
        .command()
        .env("PATH", path)
        .args(["session", "remove", SESSION_ID])
        .assert()
        .success()
        .stdout(format!(
            "removed legacy metadata {SESSION_ID}; no workload was contacted\n"
        ));
    assert!(!log.exists(), "legacy Remove must not invoke tmux");
    let migrated = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(migrated.contains(r#""version": 4"#));
    assert!(migrated.contains(r#""status": "removed""#));
    assert!(!migrated.contains("ownership_proof"));
}

#[test]
fn session_list_json_never_exposes_private_ownership_proof() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), active_state(SESSION_ID)).unwrap();

    sandbox
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(SESSION_ID))
        .stdout(predicate::str::contains("ownership_proof").not())
        .stdout(predicate::str::contains("0197f198000070008000000000000099").not());
}

#[test]
fn session_lists_hide_removed_and_order_normal_records_without_mutating_state() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    let state = r#"{"version":4,"sessions":[
{"id":"tether-0197f198000070008000000000000004","host":"local","target":"local","directory":"/removed","preset":null,"status":"removed","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-05T00:00:00Z","closed_at":"2026-01-05T00:00:00Z"},
{"id":"tether-0197f198000070008000000000000003","host":"local","target":"local","directory":"/ended","preset":null,"status":"ended","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-04T00:00:00Z","closed_at":"2026-01-04T00:00:00Z"},
{"id":"tether-0197f198000070008000000000000002","host":"local","target":"local","directory":"/older-active","preset":null,"status":"stopping","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-02T00:00:00Z","closed_at":null},
{"id":"tether-0197f198000070008000000000000001","host":"local","target":"local","directory":"/newer-active","preset":null,"status":"creating","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-03T00:00:00Z","closed_at":null}
],"orchestration_groups":[]}"#;
    fs::write(sandbox.state_file(), state).unwrap();

    let json_output = sandbox
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: serde_json::Value = serde_json::from_slice(&json_output).unwrap();
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "tether-0197f198000070008000000000000001",
            "tether-0197f198000070008000000000000002",
            "tether-0197f198000070008000000000000003",
        ]
    );

    let human = sandbox
        .command()
        .args(["session", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert_eq!(
        human
            .lines()
            .map(|line| line.split('\t').next().unwrap())
            .collect::<Vec<_>>(),
        [
            "tether-0197f198000070008000000000000001",
            "tether-0197f198000070008000000000000002",
            "tether-0197f198000070008000000000000003",
        ]
    );
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), state);
}

#[test]
fn session_lists_name_a_failing_exit_in_text_and_keep_the_status_in_json() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    let state = r#"{"version":4,"sessions":[
{"id":"tether-0197f198000070008000000000000001","host":"local","target":"local","directory":"/failed","preset":null,"status":"ended","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-03T00:00:00Z","closed_at":"2026-01-04T00:00:00Z","exit_status":2},
{"id":"tether-0197f198000070008000000000000002","host":"local","target":"local","directory":"/clean","preset":null,"status":"ended","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-03T00:00:00Z","closed_at":"2026-01-03T00:00:00Z","exit_status":0}
],"orchestration_groups":[]}"#;
    fs::write(sandbox.state_file(), state).unwrap();

    let human = sandbox
        .command()
        .args(["session", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    let statuses: Vec<&str> = human
        .lines()
        .map(|line| line.rsplit('\t').next().unwrap())
        .collect();
    assert_eq!(statuses, ["Failed (exit 2)", "Ended"]);

    // The JSON form is the state record, so it keeps the stored vocabulary and
    // reports the status as its own field rather than renaming the state.
    let json_output = sandbox
        .command()
        .args(["session", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: serde_json::Value = serde_json::from_slice(&json_output).unwrap();
    let records = listed.as_array().unwrap();
    assert_eq!(records[0]["status"], "ended");
    assert_eq!(records[0]["exit_status"], 2);
    assert_eq!(records[1]["exit_status"], 0);
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), state);
}

#[test]
fn restarting_a_workload_that_failed_immediately_is_declined_with_the_wait() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    let now = chrono::Utc::now();
    let closed_at = now - chrono::Duration::seconds(2);
    let state = serde_json::json!({
        "version": 4,
        "sessions": [{
            "id": SESSION_ID,
            "host": "local",
            "target": "local",
            "directory": "/srv/app",
            "preset": null,
            "command": "exec shell",
            "tmux_session_id": 7,
            "ownership_proof": "0197f198000070008000000000000091",
            "status": "ended",
            "created_at": (now - chrono::Duration::minutes(5)).to_rfc3339(),
            "last_used_at": (closed_at - chrono::Duration::milliseconds(400)).to_rfc3339(),
            "closed_at": closed_at.to_rfc3339(),
            "exit_status": 1
        }],
        "orchestration_groups": []
    });
    fs::write(sandbox.state_file(), state.to_string()).unwrap();
    let before = fs::read_to_string(sandbox.state_file()).unwrap();

    // The command is declined rather than deferred, the wait it reports is the
    // real remainder rather than a rounded-down zero, and it names what else to
    // do. Nothing is restarted, and the record is untouched.
    sandbox
        .command()
        .args(["session", "restart", SESSION_ID])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed immediately"))
        .stderr(predicate::str::contains("in 28s").or(predicate::str::contains("in 27s")))
        .stderr(predicate::str::contains("herdr-tether open"));
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);
}

#[test]
fn a_group_action_refuses_to_assume_consent_and_leaves_unownable_members_alone() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    let now = chrono::Utc::now();
    let legacy_id = "tether-0197f198000070008000000000000042";
    let state = serde_json::json!({
        "version": 4,
        "sessions": [
            {
                "id": SESSION_ID,
                "host": "local",
                "target": "local",
                "directory": "/srv/app",
                "preset": null,
                "command": "exec shell",
                "tmux_session_id": 7,
                "ownership_proof": "0197f198000070008000000000000091",
                "status": "running",
                "created_at": now.to_rfc3339(),
                "last_used_at": now.to_rfc3339(),
                "closed_at": null,
                "exit_status": null
            },
            {
                // A record from before ownership proofs. A group must not become
                // a way to act on a workload Tether cannot prove is its own.
                "id": legacy_id,
                "host": "local",
                "target": "local",
                "directory": "/srv/legacy",
                "preset": null,
                "command": "exec shell",
                "tmux_session_id": 8,
                "ownership_proof": null,
                "status": "running",
                "created_at": now.to_rfc3339(),
                "last_used_at": now.to_rfc3339(),
                "closed_at": null,
                "exit_status": null
            }
        ],
        "orchestration_groups": [{
            "id": "fleet",
            "title": "Fleet",
            "orchestrator_session_id": "tether-0197f198000070008000000000000009",
            "workers": [
                {
                    "session_id": SESSION_ID,
                    "membership_id": "0197f198000070008000000000000011",
                    "capabilities": {"observe_output": true, "open_interactive": true}
                },
                {
                    "session_id": legacy_id,
                    "membership_id": "0197f198000070008000000000000012",
                    "capabilities": {"observe_output": true, "open_interactive": true}
                }
            ]
        }]
    });
    fs::write(sandbox.state_file(), state.to_string()).unwrap();
    let before = fs::read_to_string(sandbox.state_file()).unwrap();

    // The plan names the legacy member as skipped and never offers to act on it.
    sandbox
        .command()
        .args(["orchestration", "stop-workers", "fleet", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "skip {legacy_id}: legacy record with no ownership proof"
        )))
        .stdout(predicate::str::contains(format!("stop {SESSION_ID}")));
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);

    // Without a terminal there is nobody to ask, so it refuses rather than
    // assuming consent for a multi-workload act.
    sandbox
        .command()
        .args(["orchestration", "stop-workers", "fleet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive confirmation"))
        .stderr(predicate::str::contains("--yes"));
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);

    // A group whose only eligible member is not restartable says so instead of
    // asking to confirm an empty act.
    sandbox
        .command()
        .args(["orchestration", "restart-workers", "fleet", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to restart"));
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);

    // A member on a host that cannot be reached fails rather than being assumed
    // gone. The summary must count only what it attempted: saying "the rest were
    // stopped" would claim the legacy record it refused to touch was.
    let mut unreachable = state.clone();
    unreachable["sessions"][0]["target"] = serde_json::json!("unreachable.invalid");
    fs::write(sandbox.state_file(), unreachable.to_string()).unwrap();
    let before = fs::read_to_string(sandbox.state_file()).unwrap();
    let failed = sandbox
        .command()
        .args(["orchestration", "stop-workers", "fleet", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1 of 1 attempted"))
        .stderr(predicate::str::contains("1 were skipped"))
        .get_output()
        .clone();
    let stderr = String::from_utf8(failed.stderr).unwrap();
    assert!(
        !stderr.contains("the rest were"),
        "a refusal is not an act: {stderr}"
    );
    // The failure names a cause rather than repeating the id, and carries none
    // of the error's source chain, which holds directories and command text.
    assert!(stderr.contains("could not prove whether"), "{stderr}");
    assert!(!stderr.contains("/srv/app"), "{stderr}");
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);

    // A `tmux` that ran but rejected the query is not evidence either, and the
    // group inherits that refusal because it calls the same operation.
    fs::write(sandbox.state_file(), state.to_string()).unwrap();
    let blind = sandbox.path("blind-bin");
    fs::create_dir_all(&blind).unwrap();
    let blind_tmux = blind.join("tmux");
    fs::write(
        &blind_tmux,
        "#!/bin/sh\necho 'unknown option -- f' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&blind_tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let before = fs::read_to_string(sandbox.state_file()).unwrap();
    let refused = sandbox
        .command()
        .env("PATH", &blind)
        .args(["orchestration", "stop-workers", "fleet", "--yes"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("could not prove whether"), "{stderr}");
    assert!(
        !stderr.contains("already gone"),
        "a server that could not be reached is not a workload that ended: {stderr}"
    );
    assert_eq!(fs::read_to_string(sandbox.state_file()).unwrap(), before);

    // Back on a reachable host, with a `tmux` that answers: the member's workload
    // is already gone, so the group reconciles it and says so, and the legacy
    // record stays untouched. The answer has to come from a server that replied -
    // a `tmux` that cannot reach one is not evidence that anything ended.
    fs::write(sandbox.state_file(), state.to_string()).unwrap();
    let bin = sandbox.path("answering-bin");
    fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    fs::write(&tmux, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    sandbox
        .command()
        .env("PATH", &bin)
        .args(["orchestration", "stop-workers", "fleet", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(already gone)"));
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sandbox.state_file()).unwrap()).unwrap();
    assert_eq!(after["sessions"][0]["status"], "ended");
    assert_eq!(
        after["sessions"][1]["ownership_proof"],
        serde_json::Value::Null,
        "the legacy record survives untouched: {after}"
    );
    assert_eq!(
        after["sessions"][1]["status"], "running",
        "a group is not a way to end a workload Tether cannot prove it owns: {after}"
    );
}

#[test]
fn external_attach_is_exact_and_does_not_mutate_state() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), active_state(SESSION_ID)).unwrap();
    let before = fs::read(sandbox.state_file()).unwrap();
    let log = sandbox.path("tmux.log");
    let body = format!("printf '%s\\n' \"$*\" >> '{}'\nexit 0", log.display());
    let (path, _) = install_tmux_script(&sandbox, &body);

    sandbox
        .command()
        .env("PATH", path)
        .args([
            "session",
            "attach-external",
            "--target",
            "local",
            "--",
            "work box",
        ])
        .assert()
        .success();

    assert_eq!(fs::read(sandbox.state_file()).unwrap(), before);
    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("attach-session -t =work box"));
    assert!(!transcript.contains("new-session"));
    assert!(!transcript.contains("kill-session"));
    assert!(!transcript.contains("list-sessions"));
}

#[test]
fn open_rejects_whitespace_directory_and_command_before_backend_create() {
    for arguments in [
        [
            "open",
            "--host",
            "local",
            "--directory",
            " \t ",
            "--command",
            "true",
        ],
        [
            "open",
            "--host",
            "local",
            "--directory",
            "/tmp",
            "--command",
            " \t ",
        ],
    ] {
        let sandbox = Sandbox::new();
        let log = sandbox.path("tmux.log");
        let body = format!("printf '%s\\n' \"$*\" >> '{}'\nexit 0", log.display());
        let (path, _) = install_tmux_script(&sandbox, &body);

        sandbox
            .command()
            .env("PATH", path)
            .args(arguments)
            .assert()
            .failure()
            .stderr(predicate::str::contains("must not be empty"));

        assert!(!log.exists(), "backend must not be invoked");
    }
}

#[test]
fn uncertain_backend_creation_preserves_private_creating_reservation() {
    let sandbox = Sandbox::new();
    let log = sandbox.path("tmux.log");
    let body = format!(
        "printf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in new-session) printf 'committed-but-malformed';; esac\nexit 0",
        log.display()
    );
    let (path, _) = install_tmux_script(&sandbox, &body);

    sandbox
        .command()
        .env("PATH", path)
        .args([
            "open",
            "--host",
            "local",
            "--directory",
            "/tmp",
            "--command",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("outcome is uncertain"));

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sandbox.state_file()).unwrap()).unwrap();
    let sessions = document["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["status"], "creating");
    assert!(sessions[0]["tmux_session_id"].is_null());
    assert!(sessions[0]["ownership_proof"].as_str().is_some());
    assert!(!fs::read_to_string(log).unwrap().contains("kill-session"));
}

#[test]
fn open_fails_closed_when_reservation_disappears_after_backend_creation() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), r#"{"version":2,"sessions":[]}"#).unwrap();
    let concurrent_id = "tether-0197f198000070008000000000000002";
    let concurrent_state = active_state(concurrent_id);
    let body = format!(
        "if [ \"$1\" = new-session ]; then printf '%s' '{}' > '{}'; printf '$7:%%3'; fi\nif [ \"$1\" = display-message ]; then printf '%s' '/tmp'; fi\nif [ \"$1\" = list-sessions ]; then filter=; for arg do filter=$arg; done; id=$(printf '%s' \"$filter\" | cut -d, -f2- | rev | cut -c2- | rev); case \"$*\" in *'#{{session_id}}'*) printf '%s:$7:0:0:' \"$id\" ;; *) printf '%s:$7:0:0:' \"$id\" ;; esac; fi\nexit 0",
        concurrent_state,
        sandbox.state_file().display()
    );
    let (path, _) = install_tmux_script(&sandbox, &body);

    sandbox
        .command()
        .env("PATH", path)
        .args([
            "open",
            "--host",
            "local",
            "--directory",
            "/tmp",
            "--command",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved session"))
        .stderr(predicate::str::contains("disappeared after creation"));

    let persisted = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(persisted.contains(concurrent_id));
    let document: serde_json::Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(document["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn herdr_context_prefers_canonical_names_and_rejects_empty_values() {
    let sandbox = Sandbox::new();
    let herdr = sandbox.path("herdr");
    fs::write(&herdr, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();

    for (name, value) in [
        ("HERDR_BIN_PATH", ""),
        ("HERDR_PANE_ID", " \t "),
        ("HERDR_WORKSPACE_ID", ""),
    ] {
        sandbox
            .command()
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_PANE_ID", "w1:p1")
            .env("HERDR_WORKSPACE_ID", "w1")
            .env(name, value)
            .args(["plugin", "open"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("empty"));
    }
}

#[test]
fn herdr_context_accepts_legacy_pane_and_workspace_fallbacks() {
    let sandbox = Sandbox::new();
    let herdr = sandbox.path("herdr");
    fs::write(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' 'herdr 0.8.0'; else printf '%s' '{\"id\":\"cli-1\",\"result\":{\"type\":\"plugin_pane_opened\"}}'; fi\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();

    sandbox
        .command()
        .env("HERDR_BIN_PATH", herdr)
        .env("PANE_ID", "w1:p1")
        .env("WORKSPACE_ID", "w1")
        .args(["plugin", "open"])
        .assert()
        .success();
}

#[test]
fn setup_reports_effective_defaults_prerequisites_and_next_action_without_mutating_herdr() {
    let sandbox = Sandbox::new();
    let (runtime_bin, herdr) = install_setup_runtime_scripts(&sandbox, None);
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    fs::write(&herdr_config, "# unchanged\n").unwrap();

    sandbox
        .command()
        .args(["host", "add", "build-box", "builder@example.test"])
        .assert()
        .success();

    let defaults = fs::read_to_string(sandbox.config_file()).unwrap();
    let configured = defaults
        .replace(
            "local_roots = []",
            "local_roots = [\"~/work\", \"/srv/local\"]",
        )
        .replace("max_depth = 4", "max_depth = 2")
        .replace("max_entries = 4096", "max_entries = 128")
        .replace("max_results = 64", "max_results = 12")
        .replace("timeout_seconds = 3", "timeout_seconds = 5")
        .replace("workers = 4", "workers = 3")
        .replace("closed_days = 30", "closed_days = 14");
    assert_ne!(configured, defaults, "the v2 defaults must be present");
    fs::write(sandbox.config_file(), configured).unwrap();

    sandbox
        .command()
        .env("PATH", &runtime_bin)
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            sandbox.config_file().display().to_string(),
        ))
        .stdout(predicate::str::contains(
            sandbox.state_file().display().to_string(),
        ))
        .stdout(predicate::str::contains("discovery"))
        .stdout(predicate::str::contains("roots ~/work, /srv/local"))
        .stdout(predicate::str::contains("depth 2"))
        .stdout(predicate::str::contains("entries 128"))
        .stdout(predicate::str::contains("results 12"))
        .stdout(predicate::str::contains("timeout 5s"))
        .stdout(predicate::str::contains("workers 3"))
        .stdout(predicate::str::contains("retention: 14 days"))
        .stdout(predicate::str::contains("placement: split-right"))
        .stdout(predicate::str::contains("Configured targets: 1"))
        .stdout(predicate::str::contains("tmux"))
        .stdout(predicate::str::contains("SSH"))
        .stdout(predicate::str::contains("Cargo").not())
        .stdout(predicate::str::contains("Herdr"))
        .stdout(predicate::str::contains("plugin_action"))
        .stdout(predicate::str::contains("herdr-tether setup keybinding"))
        .stdout(predicate::str::contains("herdr-tether open"));

    assert_eq!(fs::read_to_string(herdr_config).unwrap(), "# unchanged\n");
}

#[test]
fn prune_uses_configured_retention_unless_the_flag_explicitly_overrides_it() {
    let sandbox = Sandbox::new();
    let (runtime_bin, herdr) = install_setup_runtime_scripts(&sandbox, None);
    sandbox
        .command()
        .env("PATH", &runtime_bin)
        .env("HERDR_BIN_PATH", &herdr)
        .args(["setup", "--yes"])
        .assert()
        .success();
    let defaults = fs::read_to_string(sandbox.config_file()).unwrap();
    let configured = defaults.replace("closed_days = 30", "closed_days = 1");
    assert_ne!(
        configured, defaults,
        "the v2 retention default must be present"
    );
    fs::write(sandbox.config_file(), configured).unwrap();
    fs::write(
        sandbox.state_file(),
        format!(
            r#"{{
  "version": 2,
  "sessions": [{{
    "id": "{SESSION_ID}",
    "host": "local",
    "target": "local",
    "directory": "/tmp",
    "preset": null,
    "status": "ended",
    "created_at": "2026-07-01T00:00:00Z",
    "last_used_at": "2026-07-01T00:00:00Z",
    "closed_at": "2026-07-01T00:00:00Z"
  }}]
}}"#
        ),
    )
    .unwrap();

    sandbox
        .command()
        .args(["session", "prune", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(SESSION_ID));

    sandbox
        .command()
        .args([
            "session",
            "prune",
            "--dry-run",
            "--older-than-days",
            "99999",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    fs::write(sandbox.config_file(), "not valid TOML").unwrap();
    sandbox
        .command()
        .args([
            "session",
            "prune",
            "--dry-run",
            "--older-than-days",
            "99999",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn doctor_reports_every_check_with_restricted_path_and_missing_required_inputs() {
    let sandbox = Sandbox::new();
    let empty_path = sandbox.path("empty-bin");
    fs::create_dir_all(&empty_path).unwrap();

    sandbox
        .command()
        .env("PATH", &empty_path)
        .env("HERDR_BIN_PATH", sandbox.path("selected-herdr"))
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_WORKSPACE_ID", "w1")
        .args(["doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("config").and(predicate::str::contains("missing")))
        .stdout(predicate::str::contains("state"))
        .stdout(predicate::str::contains("tmux:"))
        .stdout(predicate::str::contains("ssh:"))
        .stdout(predicate::str::contains("cargo:"))
        .stdout(predicate::str::contains("selected-herdr"))
        .stdout(predicate::str::contains("Herdr context: w1:p1"))
        .stderr(predicate::str::contains("doctor found"));
}

#[test]
fn doctor_distinguishes_incomplete_herdr_plugin_context_from_standalone() {
    let sandbox = Sandbox::new();
    let empty_path = sandbox.path("empty-bin");
    fs::create_dir_all(&empty_path).unwrap();

    sandbox
        .command()
        .env("PATH", &empty_path)
        .env("HERDR_PANE_ID", "w1:p1")
        .args(["doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Herdr context: incomplete"))
        .stdout(predicate::str::contains("HERDR_WORKSPACE_ID"));

    sandbox
        .command()
        .env("PATH", &empty_path)
        .env("PANE_ID", "unrelated-shell-pane")
        .args(["doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "Herdr context: standalone (no plugin pane selected)",
        ));
}

#[test]
fn doctor_classifies_invalid_config_nonzero_and_permission_failures_without_short_circuiting() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config_file().parent().unwrap()).unwrap();
    fs::write(sandbox.config_file(), "version = 2\nunknown = true\n").unwrap();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), r#"{"version":2,"sessions":[]}"#).unwrap();

    let bin = sandbox.path("doctor-bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("tmux"),
        "#!/bin/sh\nprintf 'broken tmux socket\\033]0;owned\\a\\n' >&2\nexit 7\n",
    )
    .unwrap();
    fs::write(bin.join("ssh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(bin.join("cargo"), "#!/bin/sh\nexit 0\n").unwrap();
    let herdr = sandbox.path("herdr-no-execute");
    fs::write(&herdr, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        fs::set_permissions(bin.join("tmux"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o600)).unwrap();
    }

    sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_BIN_PATH", &herdr)
        .args(["doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("config").and(predicate::str::contains("unusable")))
        .stdout(predicate::str::contains("state").and(predicate::str::contains("ok")))
        .stdout(predicate::str::contains(
            "tmux: failed (exit status: 7; broken tmux socket",
        ))
        .stdout(predicate::str::contains("\u{1b}").not())
        .stdout(predicate::str::contains("ssh: ok"))
        .stdout(predicate::str::contains("cargo: ok"))
        .stdout(predicate::str::contains("permission denied"))
        .stderr(predicate::str::contains("doctor found"));
}

#[test]
fn doctor_json_uses_supported_version_flags_for_each_binary() {
    let sandbox = Sandbox::new();
    let bin = sandbox.path("doctor-bin");
    fs::create_dir_all(&bin).unwrap();

    for (executable, expected_flag) in [
        ("tmux", "-V"),
        ("ssh", "-V"),
        ("cargo", "--version"),
        ("herdr", "--version"),
    ] {
        let path = bin.join(executable);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$#\" -eq 1 ] && [ \"$1\" = {expected_flag:?} ] || exit 64\nexit 0\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let output = sandbox
        .command()
        .env("PATH", &bin)
        .args(["doctor", "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "missing config and state must still fail"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.len() < 2_048, "doctor JSON exceeded bound");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["truncated"], false);
    for name in ["tmux", "ssh", "cargo", "herdr"] {
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .unwrap();
        assert_eq!(check["status"], "ok", "wrong version argv for {name}");
        assert_eq!(check["diagnostic"], serde_json::Value::Null);
        assert_eq!(check["truncated"], false);
    }
    let protocol = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "herdr_protocol")
        .unwrap();
    assert_eq!(protocol["status"], "not_checked");
    assert_eq!(protocol["required"], false);
    assert_eq!(protocol["diagnostic"], "socket_not_provided");
    assert!(protocol.get("observed_protocol").is_none());
}

#[cfg(unix)]
#[test]
fn doctor_json_classifies_protocol_18_19_20_and_21() {
    let sandbox = Sandbox::new();
    let bin = prepare_doctor_sandbox(&sandbox);

    for (protocol, expected_status, expected_diagnostic, success) in [
        (18, "unsupported", "protocol_too_old", false),
        (19, "ok", "", true),
        (20, "ok", "", true),
        (21, "advisory", "newer_protocol_unverified", true),
    ] {
        let (socket, server) = serve_snapshot(
            &sandbox,
            &format!("herdr-protocol-{protocol}.sock"),
            protocol,
        );
        let output = sandbox
            .command()
            .env("PATH", &bin)
            .env("HERDR_SOCKET_PATH", socket)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        server.join().unwrap();

        assert_eq!(output.status.success(), success, "protocol {protocol}");
        assert!(output.stdout.len() < 2_048, "doctor JSON exceeded bound");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["completion"],
            if success { "complete" } else { "failed" }
        );
        assert_eq!(report["failure_count"], if success { 0 } else { 1 });
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "herdr_protocol")
            .unwrap();
        assert_eq!(check["status"], expected_status);
        assert_eq!(check["required"], true);
        assert_eq!(check["observed_protocol"], protocol);
        if expected_diagnostic.is_empty() {
            assert_eq!(check["diagnostic"], serde_json::Value::Null);
        } else {
            assert_eq!(check["diagnostic"], expected_diagnostic);
        }
    }
}

#[cfg(unix)]
#[test]
fn doctor_human_advises_on_new_protocol_and_rejects_old_protocol() {
    let sandbox = Sandbox::new();
    let bin = prepare_doctor_sandbox(&sandbox);

    let (socket, server) = serve_snapshot(&sandbox, "herdr-protocol-21.sock", 21);
    sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_SOCKET_PATH", socket)
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Herdr protocol: 21 newer than audited 20; continuing",
        ));
    server.join().unwrap();

    let (socket, server) = serve_snapshot(&sandbox, "herdr-protocol-18.sock", 18);
    sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_SOCKET_PATH", socket)
        .args(["doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "Herdr protocol: 18 unsupported; requires 19+",
        ));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn doctor_protocol_failures_are_typed_and_redacted() {
    let sandbox = Sandbox::new();
    let bin = prepare_doctor_sandbox(&sandbox);
    let secret_socket = sandbox.path("private-secret/socket-token.sock");

    let output = sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_SOCKET_PATH", &secret_socket)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.len() < 2_048, "doctor JSON exceeded bound");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("private-secret"), "{stdout}");
    assert!(!stdout.contains("socket-token"), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "herdr_protocol")
        .unwrap();
    assert_eq!(check["status"], "unavailable");
    assert_eq!(check["diagnostic"], "io_error");
    assert!(check.get("observed_protocol").is_none());

    let output = sandbox
        .command()
        .env("PATH", &bin)
        .env("HERDR_SOCKET_PATH", "")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "herdr_protocol")
        .unwrap();
    assert_eq!(check["status"], "incomplete");
    assert_eq!(check["diagnostic"], "missing_context");
    assert!(check.get("observed_protocol").is_none());
}

#[test]
fn doctor_json_is_stable_bounded_and_redacts_adversarial_probe_data() {
    let sandbox = Sandbox::new();
    let bin = sandbox.path("private-home/repository/doctor-bin");
    fs::create_dir_all(&bin).unwrap();
    let secret = "credential-token-DO-NOT-LEAK";
    for executable in ["tmux", "ssh", "cargo", "herdr"] {
        let path = bin.join(executable);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '\\033]0;owned\\a%s /private/home/repository host.internal %020000d' '{secret}' 1 >&2\nexit 7\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let output = sandbox
        .command()
        .env("PATH", &bin)
        .env("HOME", sandbox.path("private-home"))
        .env("API_TOKEN", secret)
        .env("HERDR_BIN_PATH", bin.join("herdr"))
        .env("HERDR_PANE_ID", "secret-session-id")
        .env("HERDR_WORKSPACE_ID", "secret-workspace-id")
        .args(["doctor", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.len() < 2_048, "doctor JSON exceeded bound");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for forbidden in [
        secret,
        "private-home",
        "repository",
        "host.internal",
        "secret-session-id",
        "secret-workspace-id",
        "\u{1b}",
        "printf",
    ] {
        assert!(
            !stdout.contains(forbidden),
            "leaked {forbidden:?}: {stdout}"
        );
    }
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["completion"], "failed");
    assert_eq!(report["truncated"], false);
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "config",
            "state",
            "tmux",
            "ssh",
            "cargo",
            "herdr",
            "herdr_protocol",
            "herdr_context"
        ]
    );
    for check in report["checks"].as_array().unwrap() {
        assert!(check["status"].is_string());
        assert!(check["required"].is_boolean());
        assert!(check["truncated"].is_boolean());
        assert!(check.get("diagnostic").is_some());
    }
}
