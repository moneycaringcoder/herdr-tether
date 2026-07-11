use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::Value;
use tempfile::TempDir;

struct Sandbox(TempDir);

impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("home/.ssh")).unwrap();
        fs::create_dir_all(temp.path().join("xdg-config/herdr-tether")).unwrap();
        fs::create_dir_all(temp.path().join("xdg-state/herdr-tether")).unwrap();
        fs::create_dir_all(temp.path().join("bin")).unwrap();
        Self(temp)
    }

    fn path(&self, path: &str) -> PathBuf {
        self.0.path().join(path)
    }

    fn command(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_herdr-tether"));
        let path = std::env::join_paths(std::iter::once(self.path("bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        command
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_STATE_HOME", self.path("xdg-state"))
            .env("PATH", path);
        command
    }

    fn install(&self, name: &str, body: &str) {
        let path = self.path(&format!("bin/{name}"));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn configure(&self) {
        fs::write(
            self.path("xdg-config/herdr-tether/config.toml"),
            r#"version = 2

[[hosts]]
name = "work"
target = "work-target"
roots = ["/z", "/a"]
presets = [{ name = "secret", command = "TOP_SECRET_COMMAND" }]

[ui]
placement = "split-right"

[discovery]
local_roots = []
max_depth = 1
max_entries = 32
max_results = 8
timeout_seconds = 10
workers = 4

[retention]
closed_days = 30
"#,
        )
        .unwrap();
        fs::write(
            self.path("home/.ssh/config"),
            "Host alias-z alias-a work * wildcard\n",
        )
        .unwrap();
        fs::write(
            self.path("xdg-state/herdr-tether/state.json"),
            r#"{"version":1,"sessions":[]}"#,
        )
        .unwrap();
        self.install("tmux", "printf 'LEAK_STDOUT\\n'");
        self.install(
            "ssh",
            "printf 'LEAK_STDOUT' >&1; printf 'LEAK_STDERR' >&2; exit 255",
        );
    }
}

#[test]
fn snapshot_schema_ordering_privacy_and_precedence() {
    let sandbox = Sandbox::new();
    sandbox.configure();
    let output = sandbox
        .command()
        .args(["snapshot"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&output).trim_end().lines().count(),
        1,
        "compact output must occupy one line"
    );
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["completion"], "partial");
    let hosts = value["hosts"].as_array().unwrap();
    let names: Vec<_> = hosts
        .iter()
        .map(|host| host["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["local", "work", "alias-z", "alias-a", "wildcard"]);
    assert_eq!(hosts[0]["origin"], "builtin");
    assert_eq!(hosts[1]["origin"], "configured");
    assert_eq!(hosts[2]["origin"], "ssh_config");
    assert_eq!(hosts[0]["reachability"]["status"], "reachable");
    assert_eq!(hosts[0]["external_catalog"]["status"], "error");
    assert_eq!(hosts[1]["reachability"]["status"], "unreachable");
    assert_eq!(hosts[1]["external_catalog"]["status"], "unavailable");
    assert_eq!(hosts[1]["discovery"]["status"], "unavailable");
    for host in hosts {
        for field in [
            "target",
            "roots",
            "repositories",
            "discovery",
            "root_errors",
            "reachability",
            "owned_sessions",
            "external_catalog",
        ] {
            assert!(host.get(field).is_some(), "missing {field}");
        }
    }
    let encoded = String::from_utf8(output).unwrap();
    for secret in ["TOP_SECRET_COMMAND", "LEAK_STDOUT", "LEAK_STDERR"] {
        assert!(!encoded.contains(secret));
    }
}

#[test]
fn pretty_is_multiline_and_legacy_json_shapes_are_unchanged() {
    let sandbox = Sandbox::new();
    sandbox.configure();
    sandbox
        .command()
        .args(["snapshot", "--pretty"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\n  \"schema_version\""));

    let host: Value = serde_json::from_slice(
        &sandbox
            .command()
            .args(["host", "list", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert!(host.is_array());
    let sessions: Value = serde_json::from_slice(
        &sandbox
            .command()
            .args(["session", "list", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert!(sessions.is_array());
}

#[test]
fn snapshot_joins_owned_status_without_mutating_state_or_lifecycle() {
    let sandbox = Sandbox::new();
    let first = sandbox.path("home/z-repo");
    let second = sandbox.path("home/a-repo");
    fs::create_dir_all(first.join(".git")).unwrap();
    fs::create_dir_all(second.join(".git")).unwrap();
    let config = r#"version = 2
hosts = []

[ui]
placement = "split-right"

[discovery]
local_roots = ["~/z-repo", "~/a-repo"]
max_depth = 1
max_entries = 32
max_results = 8
timeout_seconds = 10
workers = 2

[retention]
closed_days = 30
"#;
    let state = r#"{"version":1,"sessions":[
{"id":"tether-0197f198000070008000000000000003","host":"local","target":"local","directory":"/closed","preset":null,"status":"closed","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-03T00:00:00Z"},
{"id":"tether-0197f198000070008000000000000002","host":"local","target":"local","directory":"/closing","preset":"named","status":"closing","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-02T00:00:00Z","closed_at":null},
{"id":"tether-0197f198000070008000000000000001","host":"local","target":"local","directory":"/active","preset":null,"status":"active","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-02T00:00:00Z","closed_at":null}
]}"#;
    fs::write(sandbox.path("xdg-config/herdr-tether/config.toml"), config).unwrap();
    fs::write(sandbox.path("xdg-state/herdr-tether/state.json"), state).unwrap();
    let argv = sandbox.path("tmux.argv");
    sandbox.install("tmux", &format!(
        "printf '%s\\n' \"$*\" >> '{}'\nprintf 'z-external:0\\ntether-0197f198000070008000000000000001:2\\na-external:1\\n'",
        argv.display()
    ));

    let value: Value = serde_json::from_slice(
        &sandbox
            .command()
            .args(["snapshot"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert_eq!(value["completion"], "complete");
    let local = &value["hosts"][0];
    assert_eq!(
        local["repositories"],
        serde_json::json!([
            sandbox.path("home/a-repo").to_string_lossy(),
            sandbox.path("home/z-repo").to_string_lossy()
        ])
    );
    assert_eq!(
        local["external_catalog"]["sessions"][0]["name"],
        "a-external"
    );
    assert_eq!(
        local["external_catalog"]["sessions"][1]["name"],
        "z-external"
    );
    let owned = local["owned_sessions"].as_array().unwrap();
    assert_eq!(owned[0]["workload_status"], "running");
    assert_eq!(owned[0]["attached"], 2);
    assert_eq!(owned[1]["workload_status"], "not_checked");
    assert_eq!(owned[2]["workload_status"], "not_checked");
    assert_eq!(
        fs::read_to_string(sandbox.path("xdg-config/herdr-tether/config.toml")).unwrap(),
        config
    );
    assert_eq!(
        fs::read_to_string(sandbox.path("xdg-state/herdr-tether/state.json")).unwrap(),
        state
    );
    let invoked = fs::read_to_string(argv).unwrap();
    assert!(
        invoked
            .lines()
            .all(|line| line.starts_with("list-sessions ")),
        "unexpected non-read-only argv: {invoked}"
    );
}

#[test]
fn snapshot_reports_discovery_result_limit_as_typed_partial_data() {
    let sandbox = Sandbox::new();
    for name in ["b-repo", "a-repo"] {
        fs::create_dir_all(sandbox.path(&format!("home/{name}/.git"))).unwrap();
    }
    fs::write(
        sandbox.path("xdg-config/herdr-tether/config.toml"),
        r#"version = 2
hosts = []

[ui]
placement = "split-right"

[discovery]
local_roots = ["~"]
max_depth = 1
max_entries = 32
max_results = 1
timeout_seconds = 10
workers = 1

[retention]
closed_days = 30
"#,
    )
    .unwrap();
    fs::write(
        sandbox.path("xdg-state/herdr-tether/state.json"),
        r#"{"version":1,"sessions":[]}"#,
    )
    .unwrap();
    sandbox.install("tmux", "exit 1");

    let value: Value = serde_json::from_slice(
        &sandbox
            .command()
            .args(["snapshot"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert_eq!(value["completion"], "partial");
    assert_eq!(value["hosts"][0]["discovery"]["status"], "results_limit");
    assert_eq!(
        value["hosts"][0]["repositories"].as_array().unwrap().len(),
        1
    );
    assert_eq!(value["hosts"][0]["reachability"]["status"], "reachable");
    assert_eq!(value["hosts"][0]["external_catalog"]["status"], "available");
}

#[test]
fn snapshot_retains_removed_hosts_and_changed_targets_without_false_status() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.path("xdg-config/herdr-tether/config.toml"),
        r#"version = 2
[[hosts]]
name = "work"
target = "new-target"
roots = []
presets = []

[ui]
placement = "split-right"
[discovery]
local_roots = []
max_depth = 1
max_entries = 32
max_results = 8
timeout_seconds = 1
workers = 2
[retention]
closed_days = 30
"#,
    )
    .unwrap();
    fs::write(sandbox.path("xdg-state/herdr-tether/state.json"), r#"{"version":1,"sessions":[
{"id":"tether-0197f198000070008000000000000011","host":"removed","target":"old-removed","directory":"/removed","preset":null,"status":"active","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-01T00:00:00Z","closed_at":null},
{"id":"tether-0197f198000070008000000000000012","host":"work","target":"old-target","directory":"/old-work","preset":null,"status":"active","created_at":"2026-01-01T00:00:00Z","last_used_at":"2026-01-01T00:00:00Z","closed_at":null}
]}"#).unwrap();
    sandbox.install("tmux", "exit 1");
    sandbox.install("ssh", "exit 255");

    let value: Value = serde_json::from_slice(
        &sandbox
            .command()
            .args(["snapshot"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert_eq!(value["completion"], "partial");
    let hosts = value["hosts"].as_array().unwrap();
    let identity: Vec<_> = hosts
        .iter()
        .map(|host| {
            (
                host["name"].as_str().unwrap(),
                host["origin"].as_str().unwrap(),
                host["target"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        identity,
        [
            ("local", "builtin", None),
            ("work", "configured", Some("new-target")),
            ("removed", "state", Some("old-removed")),
            ("work", "state", Some("old-target")),
        ]
    );
    assert!(hosts[1]["owned_sessions"].as_array().unwrap().is_empty());
    for host in &hosts[2..] {
        assert_eq!(host["discovery"]["status"], "not_collected");
        assert_eq!(host["reachability"]["status"], "not_collected");
        assert_eq!(host["external_catalog"]["status"], "not_collected");
        assert_eq!(
            host["owned_sessions"][0]["workload_status"],
            "not_collected"
        );
    }
    let ids: Vec<_> = hosts
        .iter()
        .flat_map(|host| host["owned_sessions"].as_array().unwrap())
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "tether-0197f198000070008000000000000011",
            "tether-0197f198000070008000000000000012",
        ]
    );
}

#[test]
fn snapshot_slow_ssh_is_bounded_and_returns_valid_partial_json() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.path("xdg-config/herdr-tether/config.toml"),
        r#"version = 2
[[hosts]]
name = "slow"
target = "slow-target"
roots = []
presets = []

[ui]
placement = "split-right"
[discovery]
local_roots = []
max_depth = 1
max_entries = 32
max_results = 8
timeout_seconds = 1
workers = 1
[retention]
closed_days = 30
"#,
    )
    .unwrap();
    fs::write(
        sandbox.path("xdg-state/herdr-tether/state.json"),
        r#"{"version":1,"sessions":[]}"#,
    )
    .unwrap();
    sandbox.install("tmux", "exit 1");
    let pids = sandbox.path("ssh.pids");
    sandbox.install(
        "ssh",
        &format!(
            "sleep 30 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\" >> '{}'\nwait",
            pids.display()
        ),
    );

    let started = std::time::Instant::now();
    let output = sandbox
        .command()
        .args(["snapshot"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["completion"], "partial");
    assert!(matches!(
        value["hosts"][1]["reachability"]["status"].as_str(),
        Some("timed_out" | "not_collected")
    ));
    assert_eq!(value["hosts"][1]["discovery"]["status"], "timed_out");
    for pid in fs::read_to_string(pids).unwrap().lines() {
        assert!(
            !PathBuf::from("/proc").join(pid).exists(),
            "snapshot returned before SSH process {pid} was reaped"
        );
    }
}
