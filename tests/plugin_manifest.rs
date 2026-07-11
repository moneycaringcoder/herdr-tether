use std::fs;

#[test]
fn manifest_declares_build_actions_and_managed_overlay_panes() {
    let manifest = fs::read_to_string("herdr-plugin.toml").expect("herdr-plugin.toml is present");
    let value: toml::Value = toml::from_str(&manifest).expect("manifest is valid TOML");

    assert_eq!(value["id"].as_str(), Some("moneycaringcoder.tether"));
    assert_eq!(value["name"].as_str(), Some("Tether for Herdr"));
    assert_eq!(value["version"].as_str(), Some("0.2.0"));
    assert_eq!(value["min_herdr_version"].as_str(), Some("0.7.3"));
    assert_eq!(
        value["platforms"].as_array().unwrap(),
        &[
            toml::Value::String("linux".into()),
            toml::Value::String("macos".into())
        ]
    );

    let build = value["build"].as_array().unwrap();
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
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["id"].as_str(), Some("open"));
    assert_eq!(actions[0]["title"].as_str(), Some("Tether: Open"));
    assert_eq!(
        actions[0]["command"].as_array().unwrap(),
        &[
            toml::Value::String("target/release/herdr-tether".into()),
            toml::Value::String("plugin".into()),
            toml::Value::String("open".into()),
        ]
    );
    assert_eq!(actions[1]["id"].as_str(), Some("setup"));

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
    assert_eq!(panes[1]["placement"].as_str(), Some("overlay"));
    assert_eq!(
        panes[1]["command"].as_array().unwrap(),
        &[
            toml::Value::String("sh".into()),
            toml::Value::String("-c".into()),
            toml::Value::String(
                "exec \"${HERDR_PLUGIN_ROOT:?}/target/release/herdr-tether\" setup".into(),
            ),
        ]
    );
}
