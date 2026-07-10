use std::{fs, path::Path, process::Command};

use predicates::prelude::*;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct Sandbox {
    temp: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().unwrap(),
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
            .env_remove("HERDR_TAB_ID");
        command
    }

    fn config_file(&self) -> std::path::PathBuf {
        self.path("xdg-config/herdr-tether/config.toml")
    }

    fn state_file(&self) -> std::path::PathBuf {
        self.path("xdg-state/herdr-tether/state.json")
    }
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
    fs::write(ssh_dir.join("config"), "Host from-ssh-config\n  HostName 192.0.2.20\n").unwrap();

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
        fs::metadata(sandbox.config_file()).unwrap().permissions().mode() & 0o777,
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
        "#!/bin/sh\nprintf '%s' \"$1\" >> '{log}'\nshift\nfor arg do printf ' <%s>' \"$arg\" >> '{log}'; done\nprintf '\\n' >> '{log}'\ncase \"$(tail -n 1 '{log}')\" in\n  display-message*) printf '0' ;;\nesac\nexit 0\n",
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
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
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
        .args([
            "session",
            "prune",
            "--dry-run",
            "--older-than-days",
            "0",
        ])
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
    assert!(transcript.lines().all(|line| line.contains("<BatchMode=yes>")));
    assert!(!transcript.contains("StrictHostKeyChecking"));
    assert!(!transcript.contains("herdr --remote"));
}
