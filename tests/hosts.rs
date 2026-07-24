use std::fs;

use herdr_tether::{
    config::{CommandPreset, Config, HostConfig},
    sshcfg::{discover_aliases, validate_ssh_target},
};
use tempfile::tempdir;

fn host(name: &str, target: &str) -> HostConfig {
    HostConfig {
        name: name.into(),
        target: target.into(),
        roots: vec!["/srv/code with spaces".into()],
        presets: vec![CommandPreset {
            herdr_agent: None,
            name: "shell".into(),
            command: "exec ${SHELL:-/bin/sh} -l".into(),
        }],
    }
}

#[test]
fn discovers_only_literal_openssh_host_aliases() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("config");
    fs::write(
        &path,
        r#"
# Tether must ignore full-line comments and wildcard blocks.
   # An indented comment is still a comment.
Host build-box code.example.test
    HostName 192.0.2.10
Host hash#alias # trailing text is aliases not an inline comment
Host *.internal !bastion
    User deploy
host laptop
    HostName 192.0.2.11
Host build-box
    User duplicate-is-deduplicated
Match host *
    User ignored
"#,
    )
    .unwrap();

    assert_eq!(
        discover_aliases(&path).unwrap(),
        [
            "build-box",
            "code.example.test",
            "hash#alias",
            "#",
            "trailing",
            "text",
            "is",
            "aliases",
            "not",
            "an",
            "inline",
            "comment",
            "laptop",
        ]
    );
}

#[test]
fn discovers_bounded_includes_without_escaping_the_primary_ssh_directory() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join(".ssh");
    let includes = ssh.join("config.d");
    fs::create_dir_all(&includes).unwrap();
    let outside = temp.path().join("outside.conf");
    fs::write(&outside, "Host outside-must-not-appear\n").unwrap();
    fs::write(
        ssh.join("config"),
        format!(
            "Host root\nInclude config.d/*.conf ../outside.conf {}\n",
            outside.display()
        ),
    )
    .unwrap();
    fs::write(
        includes.join("10-base.conf"),
        "Host included-a *.pattern\nInclude nested.conf ../config\n",
    )
    .unwrap();
    fs::write(ssh.join("nested.conf"), "Host nested\nInclude config\n").unwrap();
    fs::write(
        includes.join("20-extra.conf"),
        "Host included-b included-a\n",
    )
    .unwrap();

    assert_eq!(
        discover_aliases(&ssh.join("config")).unwrap(),
        ["root", "included-a", "nested", "included-b"]
    );
}

#[test]
fn a_missing_ssh_config_yields_an_empty_catalog() {
    let temp = tempdir().unwrap();
    assert!(
        discover_aliases(&temp.path().join("missing"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn explicit_targets_accept_alias_user_host_and_lowercase_ssh_uri() {
    for target in [
        "build-box",
        "hash#alias",
        "builder@example.test",
        "ssh://builder@example.test:2222",
        "ssh://builder@[2001:db8::10]:2222",
    ] {
        validate_ssh_target(target).unwrap();
    }

    for target in [
        "",
        "-oProxyCommand=touch-nope",
        "host name",
        "host;touch-nope",
        "host\nname",
        "ssh://",
        "SSH://builder@example.test",
        "http://builder@example.test",
    ] {
        assert!(validate_ssh_target(target).is_err(), "accepted {target:?}");
    }
}

#[test]
fn hosts_are_added_and_removed_without_hand_editing() {
    let mut config = Config::default();
    config
        .add_host(host("build-box", "builder@example.test"))
        .unwrap();
    assert_eq!(config.hosts.len(), 1);

    let duplicate = config
        .add_host(host("build-box", "other.example.test"))
        .unwrap_err()
        .to_string();
    assert!(duplicate.contains("already exists"));

    assert!(config.remove_host("build-box"));
    assert!(!config.remove_host("build-box"));
    assert!(config.hosts.is_empty());
}

#[test]
fn config_validation_rejects_reserved_names_and_nul_data() {
    let mut config = Config::default();
    config.hosts.push(host("local", "example.test"));
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );

    let mut config = Config::default();
    let mut invalid = host("build-box", "example.test");
    invalid.presets[0].command = "echo ok\0touch nope".into();
    config.hosts.push(invalid);
    assert!(config.validate().unwrap_err().to_string().contains("NUL"));
}
