use std::{fs, process::Command};

#[test]
fn manifest_declares_build_actions_and_managed_overlay_panes() {
    let manifest = fs::read_to_string("herdr-plugin.toml").expect("herdr-plugin.toml is present");
    let value: toml::Value = toml::from_str(&manifest).expect("manifest is valid TOML");

    assert_eq!(value["id"].as_str(), Some("moneycaringcoder.tether"));
    assert_eq!(value["name"].as_str(), Some("Tether for Herdr"));
    assert_eq!(value["version"].as_str(), Some("0.3.0"));
    assert_eq!(value["min_herdr_version"].as_str(), Some("0.7.3"));
    assert_eq!(
        value["description"].as_str(),
        Some("Keep local and remote terminal workloads running after their Herdr view closes.")
    );
    let supported_top_level_fields = [
        "id",
        "name",
        "version",
        "min_herdr_version",
        "description",
        "platforms",
        "build",
        "actions",
        "events",
        "panes",
        "link_handlers",
    ];
    assert!(
        value
            .as_table()
            .unwrap()
            .keys()
            .all(|field| supported_top_level_fields.contains(&field.as_str())),
        "manifest contains a field outside Herdr's documented schema"
    );
    assert_eq!(
        value["platforms"].as_array().unwrap(),
        &[
            toml::Value::String("linux".into()),
            toml::Value::String("macos".into())
        ]
    );

    let build = value["build"].as_array().unwrap();
    assert!(
        build[0]
            .as_table()
            .unwrap()
            .keys()
            .all(|field| ["command", "platforms"].contains(&field.as_str())),
        "build entry contains a field outside Herdr's documented schema"
    );
    assert_eq!(
        build[0]["command"].as_array().unwrap(),
        &[
            toml::Value::String("cargo".into()),
            toml::Value::String("build".into()),
            toml::Value::String("--release".into()),
            toml::Value::String("--locked".into()),
        ]
    );

    let actions = value["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 3);
    assert!(
        actions[0].as_table().unwrap().keys().all(|field| {
            ["id", "title", "description", "command", "contexts"].contains(&field.as_str())
        }),
        "open action contains a field outside Herdr's documented schema"
    );
    assert_eq!(actions[0]["id"].as_str(), Some("open"));
    assert_eq!(actions[0]["title"].as_str(), Some("Tether: Open workloads"));
    assert_eq!(
        actions[0]["description"].as_str(),
        Some("Create, open, or restart local and remote workloads. Run Tether: Set up first.")
    );
    assert_eq!(
        actions[0]["command"].as_array().unwrap(),
        &[
            toml::Value::String("target/release/herdr-tether".into()),
            toml::Value::String("plugin".into()),
            toml::Value::String("open".into()),
        ]
    );
    assert_eq!(
        actions[0]["contexts"].as_array().unwrap(),
        &[
            toml::Value::String("global".into()),
            toml::Value::String("workspace".into()),
            toml::Value::String("pane".into()),
        ]
    );
    assert_eq!(actions[1]["id"].as_str(), Some("setup"));
    assert_eq!(
        actions[1]["title"].as_str(),
        Some("Tether: Set up (start here)")
    );
    assert_eq!(
        actions[1]["description"].as_str(),
        Some("Create Tether's private files and safely add the prefix+t shortcut.")
    );
    assert_eq!(actions[2]["id"].as_str(), Some("rollback"));
    assert_eq!(
        actions[2]["title"].as_str(),
        Some("Tether: Remove prefix+t shortcut")
    );
    assert_eq!(
        actions[2]["description"].as_str(),
        Some("Restore the exact Herdr configuration saved by Tether setup.")
    );
    assert_eq!(
        actions[2]["command"].as_array().unwrap(),
        &[
            toml::Value::String("target/release/herdr-tether".into()),
            toml::Value::String("setup".into()),
            toml::Value::String("keybinding".into()),
            toml::Value::String("--rollback".into()),
        ]
    );

    let panes = value["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0]["id"].as_str(), Some("picker"));
    assert_eq!(panes[0]["placement"].as_str(), Some("overlay"));
    assert_eq!(
        panes[0]["command"].as_array().unwrap(),
        &[
            toml::Value::String("sh".into()),
            toml::Value::String("-c".into()),
            toml::Value::String(
                "exec \"${HERDR_PLUGIN_ROOT:?}/target/release/herdr-tether\" open".into(),
            ),
        ]
    );
    assert_eq!(panes[1]["id"].as_str(), Some("setup"));
    assert_eq!(panes[1]["title"].as_str(), Some("Tether: Set up"));
    assert_eq!(panes[1]["placement"].as_str(), Some("overlay"));
    let setup_command = panes[1]["command"].as_array().unwrap();
    let shell = setup_command[2].as_str().unwrap();
    assert!(shell.contains("setup --yes"));
    assert!(shell.contains("setup keybinding"));
    assert!(shell.contains("Press Enter"));
    assert!(shell.contains("read"));
    assert!(shell.contains("exit \"$status\""));
}

#[cfg(unix)]
fn run_setup_pane(keybinding_status: i32) -> (std::process::Output, String) {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("target/release/herdr-tether");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(
        &binary,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HERDR_PLUGIN_ROOT/invocations\"\ncase \"$*\" in\n  'setup keybinding') exit \"${TETHER_TEST_KEYBINDING_STATUS:-0}\" ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();

    let manifest = fs::read_to_string("herdr-plugin.toml").unwrap();
    let value: toml::Value = toml::from_str(&manifest).unwrap();
    let shell = value["panes"][1]["command"][2].as_str().unwrap();
    let output = Command::new("sh")
        .args(["-c", shell])
        .env("HERDR_PLUGIN_ROOT", root.path())
        .env(
            "TETHER_TEST_KEYBINDING_STATUS",
            keybinding_status.to_string(),
        )
        .output()
        .unwrap();
    let invocations = fs::read_to_string(root.path().join("invocations")).unwrap();
    (output, invocations)
}

#[cfg(unix)]
#[test]
fn setup_pane_keeps_success_and_failure_outcomes_visible_and_actionable() {
    let (success, invocations) = run_setup_pane(0);
    assert!(success.status.success());
    assert_eq!(invocations, "setup --yes\nsetup keybinding\n");
    let stdout = String::from_utf8(success.stdout).unwrap();
    assert!(stdout.contains("Tether is ready"));
    assert!(stdout.contains("prefix+t"));
    assert!(stdout.contains("Press Enter"));

    let (failure, invocations) = run_setup_pane(17);
    assert_eq!(failure.status.code(), Some(17));
    assert_eq!(invocations, "setup --yes\nsetup keybinding\n");
    let stdout = String::from_utf8(failure.stdout).unwrap();
    assert!(stdout.contains("setup did not finish"));
    assert!(stdout.contains("Read the error above"));
    assert!(stdout.contains("Press Enter"));
}
