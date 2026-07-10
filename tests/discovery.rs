use std::{fs, path::Path, time::Duration};

use herdr_tether::{
    backend::ProcessBinaries,
    discovery::{
        DiscoveryCompletion, DiscoveryLimits, DiscoveryLocation, DiscoveryMessage,
        DiscoveryRequest, DiscoveryService,
    },
};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn repo(path: &Path, git_file: bool) {
    fs::create_dir_all(path).unwrap();
    if git_file {
        fs::write(path.join(".git"), "gitdir: elsewhere").unwrap();
    } else {
        fs::create_dir(path.join(".git")).unwrap();
    }
}

fn limits() -> DiscoveryLimits {
    DiscoveryLimits {
        max_depth: 4,
        max_entries: 100,
        max_results: 10,
        timeout: Duration::from_secs(1),
        workers: 2,
    }
}

fn collect(service: &DiscoveryService, request: DiscoveryRequest) -> Vec<DiscoveryMessage> {
    let generation = request.generation;
    let run = service.start(request);
    let mut messages = Vec::new();
    loop {
        let message = run.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let finished = matches!(
            message,
            DiscoveryMessage::Finished {
                generation: value
            } if value == generation
        );
        messages.push(message);
        if finished {
            return messages;
        }
    }
}

#[cfg(unix)]
fn collect_remote_output(output_command: &str, generation: u64) -> Vec<DiscoveryMessage> {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(&ssh, format!("#!/bin/sh\n{output_command}\n")).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        limits(),
    );
    collect(
        &service,
        DiscoveryRequest {
            generation,
            locations: vec![DiscoveryLocation {
                host: "dev".into(),
                target: Some("dev.example".into()),
                roots: vec!["/safe".into()],
            }],
        },
    )
}

#[test]
fn local_discovery_is_lexical_deduplicated_and_prunes_repositories() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    let alpha = root.join("alpha");
    let zeta = root.join("zeta");
    repo(&zeta, false);
    repo(&alpha, true);
    repo(&alpha.join("nested-must-be-pruned"), false);
    let outside = temp.path().join("outside");
    repo(&outside, false);
    #[cfg(unix)]
    symlink(&outside, root.join("outside-link")).unwrap();

    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), limits());
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 1,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![
                    root.to_string_lossy().into_owned(),
                    alpha.to_string_lossy().into_owned(),
                ],
            }],
        },
    );

    let repositories: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            DiscoveryMessage::Repository { host, path, .. } if host == "local" => {
                Some(path.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        repositories,
        [
            alpha.to_string_lossy().into_owned(),
            zeta.to_string_lossy().into_owned()
        ]
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            host,
            completion: DiscoveryCompletion::Complete,
            ..
        } if host == "local"
    )));
}

#[test]
fn repository_root_is_emitted_without_a_trailing_separator() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("repository");
    repo(&root, false);
    let root_text = root.to_string_lossy().into_owned();
    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), limits());
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 10,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![root_text.clone()],
            }],
        },
    );

    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::Repository { path, .. } if path == &root_text
    )));
}
#[test]
fn local_discovery_enforces_result_bound() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    repo(&root.join("a"), false);
    repo(&root.join("b"), false);
    let mut bounded = limits();
    bounded.max_results = 1;
    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), bounded);
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 2,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, DiscoveryMessage::Repository { .. }))
            .count(),
        1
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::ResultsLimit,
            ..
        }
    )));
}

#[test]
fn exact_local_result_limit_is_complete() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    repo(&root.join("only"), false);
    let mut bounded = limits();
    bounded.max_results = 1;
    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), bounded);
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 20,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::Complete,
            ..
        }
    )));
}

#[cfg(unix)]
#[test]
fn local_symlink_root_is_an_explicit_error() {
    let temp = tempdir().unwrap();
    let repository = temp.path().join("repository");
    repo(&repository, false);
    let root = temp.path().join("root-link");
    symlink(&repository, &root).unwrap();
    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), limits());
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 21,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, DiscoveryMessage::Repository { .. }))
    );
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, DiscoveryMessage::RootError { .. }))
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::Error,
            ..
        }
    )));
}

#[cfg(unix)]
#[test]
fn remote_discovery_quotes_roots_and_parses_framed_results() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let log = temp.path().join("argv.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\nprintf 'R\\0000\\000repo with space\\000'\n",
        log.display()
    );
    fs::write(&ssh, script).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        limits(),
    );
    let malicious_root = "/srv/a b/'$(touch nope)'";
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 3,
            locations: vec![DiscoveryLocation {
                host: "dev".into(),
                target: Some("dev.example".into()),
                roots: vec![malicious_root.into()],
            }],
        },
    );

    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::Repository { host, path, .. }
            if host == "dev" && path == "/srv/a b/'$(touch nope)'/repo with space"
    )));
    let argv = fs::read(log).unwrap();
    let arguments: Vec<&[u8]> = argv
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect();
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair[0] == b"--" && pair[1] == b"dev.example")
    );
    assert_eq!(
        arguments
            .last()
            .unwrap()
            .iter()
            .filter(|byte| **byte == b'\'')
            .count()
            % 2,
        0
    );
    assert!(!temp.path().join("nope").exists());
}

#[cfg(unix)]
#[test]
fn remote_parser_rejects_unsafe_and_malformed_records() {
    let cases = [
        "printf 'R\\0000\\000../escape\\000'",
        "printf 'R\\0000\\000/etc\\000'",
        "printf 'R\\0001\\000repo\\000'",
        "printf 'R\\0000\\000repo'",
    ];
    for (index, output) in cases.into_iter().enumerate() {
        let messages = collect_remote_output(output, 30 + index as u64);
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, DiscoveryMessage::Repository { .. }))
        );
        assert!(messages.iter().any(|message| matches!(
            message,
            DiscoveryMessage::HostFinished {
                completion: DiscoveryCompletion::Malformed,
                ..
            }
        )));
    }
}

#[cfg(unix)]
#[test]
fn remote_parser_keeps_valid_results_beside_non_utf8_records() {
    let messages = collect_remote_output(
        "printf 'R\\0000\\000good\\000R\\0000\\000bad\\377name\\000'",
        40,
    );

    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::Repository { path, .. } if path == "/safe/good"
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::Error,
            ..
        }
    )));
}

#[cfg(unix)]
#[test]
fn remote_scanner_frames_whole_records_at_the_result_limit() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root with spaces");
    repo(&root.join("alpha"), false);
    repo(&root.join("zeta"), false);
    let outside = temp.path().join("outside");
    repo(&outside, false);
    symlink(&outside, root.join("aaa-outside-link")).unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do command=$argument; done\nexec /bin/sh -c \"$command\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bounded = limits();
    bounded.max_results = 1;
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        bounded,
    );
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 4,
            locations: vec![DiscoveryLocation {
                host: "dev".into(),
                target: Some("dev.example".into()),
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, DiscoveryMessage::Repository { .. }))
            .count(),
        1
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::Repository { path, .. } if path == &root.join("alpha").to_string_lossy()
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::ResultsLimit,
            ..
        }
    )));
}

#[cfg(unix)]
#[test]
fn remote_scanner_enforces_the_filesystem_entry_limit() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    repo(&root.join("alpha"), false);
    repo(&root.join("zeta"), false);
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do command=$argument; done\nexec /bin/sh -c \"$command\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bounded = limits();
    bounded.max_entries = 2;
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        bounded,
    );
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 5,
            locations: vec![DiscoveryLocation {
                host: "dev".into(),
                target: Some("dev.example".into()),
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, DiscoveryMessage::Repository { .. }))
            .count(),
        1
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::EntriesLimit,
            ..
        }
    )));
}
