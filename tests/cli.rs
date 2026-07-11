use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use predicates::prelude::*;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
            .env_remove("HERDR_BIN_PATH")
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

const SESSION_ID: &str = "tether-0197f198000070008000000000000001";

fn active_state(id: &str) -> String {
    format!(
        r#"{{
  "version": 1,
  "sessions": [{{
    "id": "{id}",
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
    )
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

#[test]
fn every_scriptable_surface_has_help() {
    let sandbox = Sandbox::new();
    let commands: &[&[&str]] = &[
        &["--help"],
        &["setup", "--help"],
        &["host", "--help"],
        &["host", "add", "--help"],
        &["host", "list", "--help"],
        &["host", "remove", "--help"],
        &["host", "check", "--help"],
        &["open", "--help"],
        &["session", "--help"],
        &["session", "list", "--help"],
        &["session", "resume", "--help"],
        &["session", "close", "--help"],
        &["session", "prune", "--help"],
        &["doctor", "--help"],
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
fn plugin_directories_override_xdg_and_setup_never_edits_herdr_config() {
    let sandbox = Sandbox::new();
    let plugin_config = sandbox.path("plugin-config");
    let plugin_state = sandbox.path("plugin-state");
    let herdr_config = sandbox.path("xdg-config/herdr/config.toml");
    fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
    fs::write(&herdr_config, "# sentinel\n").unwrap();

    sandbox
        .command()
        .env("HERDR_PLUGIN_CONFIG_DIR", &plugin_config)
        .env("HERDR_PLUGIN_STATE_DIR", &plugin_state)
        .args(["setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plugin_action"))
        .stdout(predicate::str::contains("moneycaringcoder.tether.open"));

    assert!(plugin_config.join("config.toml").exists());
    assert!(plugin_state.join("state.json").exists());
    assert_eq!(fs::read_to_string(herdr_config).unwrap(), "# sentinel\n");
    assert!(!sandbox.config_file().exists());
}

fn write_fake_tmux(path: &Path, log: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf '%s' \"$1\" >> '{log}'\nshift\nfor arg do printf ' <%s>' \"$arg\" >> '{log}'; done\nprintf '\\n' >> '{log}'\ncase \"$(tail -n 1 '{log}')\" in\n  list-sessions*) filter=$(tail -n 1 '{log}' | sed -n 's/.* <\\(#[^>]*\\)>$/\\1/p'); id=$(printf '%s' \"$filter\" | cut -d, -f2- | rev | cut -c2- | rev); printf '%s\\t0' \"$id\" ;;\nesac\nexit 0\n",
        log = log.display()
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
        .args(["session", "resume", &session_id])
        .assert()
        .success();
    sandbox
        .command()
        .env("PATH", &path)
        .args(["session", "close", &session_id])
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
        .success();
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
        "#!/bin/sh\nfor arg do printf '<%s>' \"$arg\" >> '{log}'; done\nprintf '\\n' >> '{log}'\ncase \"$*\" in\n  *\"tmux\"*\"-V\"*) printf 'tmux 3.4\\n' ;;\n  *\"herdr\"*\"--version\"*) printf 'herdr 0.7.3\\n' ;;\nesac\nexit 0\n",
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
        .stdout(predicate::str::contains("herdr 0.7.3"));

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
fn close_marks_a_missing_workload_closed_without_killing_it() {
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
        .args(["session", "close", SESSION_ID])
        .assert()
        .success()
        .stdout(predicate::str::contains("closed"));

    let transcript = fs::read_to_string(log).unwrap();
    assert!(transcript.contains("list-sessions"));
    assert!(!transcript.contains("kill-session"));
    let persisted = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(persisted.contains(r#""status": "closed""#));
    assert!(!persisted.contains(r#""closed_at": null"#));
}

#[test]
fn close_unknown_or_failed_running_workload_preserves_active_metadata() {
    for (name, body) in [
        (
            "unknown",
            "case \"$1\" in\n  list-sessions) printf 'malformed'; exit 0 ;;\n  kill-session) exit 99 ;;\nesac\nexit 0",
        ),
        (
            "failed-running-close",
            "case \"$1\" in\n  list-sessions) printf 'tether-0197f198000070008000000000000001\\t0'; exit 0 ;;\n  kill-session) printf 'still running' >&2; exit 2 ;;\nesac\nexit 0",
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
            .args(["session", "close", SESSION_ID])
            .assert()
            .failure();

        let after = fs::read(sandbox.state_file()).unwrap();
        if name == "unknown" {
            assert_eq!(after, before, "unknown state must remain unchanged");
        } else {
            let persisted = String::from_utf8(after).unwrap();
            assert!(
                persisted.contains(r#""status": "closing""#),
                "failed close must persist a recoverable closing marker"
            );
            let document: serde_json::Value = serde_json::from_str(&persisted).unwrap();
            assert!(document["sessions"][0]["closed_at"].is_null());
        }
        let transcript = fs::read_to_string(log).unwrap();
        if name == "unknown" {
            assert!(!transcript.contains("kill-session"));
        } else {
            assert!(transcript.contains("kill-session"));
        }
    }
}

#[test]
fn resume_rejects_missing_unknown_and_closed_sessions_without_mutation() {
    for (name, state, body, expected) in [
        (
            "missing",
            active_state(SESSION_ID),
            "case \"$1\" in list-sessions) exit 0;; esac\nexit 0",
            "no longer exists",
        ),
        (
            "unknown",
            active_state(SESSION_ID),
            "case \"$1\" in list-sessions) printf 'malformed'; exit 0;; esac\nexit 0",
            "could not determine",
        ),
        (
            "closing",
            active_state(SESSION_ID).replace(r#""status": "active""#, r#""status": "closing""#),
            "exit 99",
            "is closing",
        ),
        (
            "closed",
            active_state(SESSION_ID)
                .replace(r#""status": "active""#, r#""status": "closed""#)
                .replace(
                    r#""closed_at": null"#,
                    r#""closed_at": "2026-01-01T00:00:01Z""#,
                ),
            "exit 99",
            "is closed",
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
            .args(["session", "resume", SESSION_ID])
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
fn open_reloads_state_after_backend_creation_before_persisting() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.state_file().parent().unwrap()).unwrap();
    fs::write(sandbox.state_file(), r#"{"version":1,"sessions":[]}"#).unwrap();
    let concurrent_id = "tether-0197f198000070008000000000000002";
    let concurrent_state = active_state(concurrent_id);
    let body = format!(
        "if [ \"$1\" = new-session ]; then printf '%s' '{}' > '{}'; fi\nexit 0",
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
        .success();

    let persisted = fs::read_to_string(sandbox.state_file()).unwrap();
    assert!(persisted.contains(concurrent_id));
    let document: serde_json::Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(document["sessions"].as_array().unwrap().len(), 2);
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
        "#!/bin/sh\nprintf '%s' '{\"id\":\"cli-1\",\"result\":{\"type\":\"plugin_pane_opened\"}}'\n",
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
