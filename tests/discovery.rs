use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use herdr_tether::{
    backend::ProcessBinaries,
    discovery::{
        DiscoveryCompletion, DiscoveryLimits, DiscoveryLocation, DiscoveryMessage,
        DiscoveryRequest, DiscoveryRun, DiscoveryRunCompletion, DiscoveryService,
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
    collect_messages(&run, generation)
}

fn collect_messages(run: &DiscoveryRun, generation: u64) -> Vec<DiscoveryMessage> {
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
    let mut test_limits = limits();
    test_limits.timeout = Duration::from_secs(10);
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        test_limits,
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
fn separate_git_dir_and_submodule_layouts_offer_checkouts_and_not_git_directories() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    // `--separate-git-dir`: the checkout keeps a `.git` file and its Git
    // directory lives elsewhere under the same scan root, where a scan will
    // walk straight into it.
    let checkout = root.join("app");
    let separate = root.join("gitdirs/app");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(separate.join("worktrees/app-feature")).unwrap();
    fs::write(separate.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", separate.display()),
    )
    .unwrap();
    // A submodule: a `.git` file with a relative target inside the
    // superproject's own Git directory.
    let superproject = root.join("super");
    let submodule = superproject.join("lib");
    fs::create_dir_all(&submodule).unwrap();
    fs::create_dir_all(superproject.join(".git/modules/lib")).unwrap();
    fs::write(submodule.join(".git"), "gitdir: ../.git/modules/lib\n").unwrap();

    let service = DiscoveryService::new(ProcessBinaries::new("ssh", "tmux"), limits());
    let messages = collect(
        &service,
        DiscoveryRequest {
            generation: 1,
            locations: vec![DiscoveryLocation {
                host: "local".into(),
                target: None,
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    let repositories: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            DiscoveryMessage::Repository { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    // Both `.git` files are recognized, the superproject prunes its submodule
    // the way it prunes any nested repository, and no Git directory is offered
    // as somewhere to work.
    assert_eq!(
        repositories,
        [
            checkout.to_string_lossy().into_owned(),
            superproject.to_string_lossy().into_owned()
        ]
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            completion: DiscoveryCompletion::Complete,
            ..
        }
    )));

    // The same distinction, directly: a checkout holds a `.git` entry whether
    // it is a file or a directory, and a Git directory holds none.
    assert!(herdr_tether::discovery::is_checkout_directory(&checkout));
    assert!(herdr_tether::discovery::is_checkout_directory(&submodule));
    assert!(herdr_tether::discovery::is_checkout_directory(
        &superproject
    ));
    assert!(!herdr_tether::discovery::is_checkout_directory(&separate));
    assert!(!herdr_tether::discovery::is_checkout_directory(
        &separate.join("worktrees/app-feature")
    ));
    assert!(!herdr_tether::discovery::is_checkout_directory(
        &superproject.join(".git/modules/lib")
    ));
    assert!(!herdr_tether::discovery::is_checkout_directory(
        &root.join("missing")
    ));
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
                target: Some("ssh://builder@dev.example:2222".into()),
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
            .any(|pair| pair[0] == b"-p" && pair[1] == b"2222")
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair[0] == b"--" && pair[1] == b"builder@dev.example")
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
        assert!(
            messages.iter().any(|message| matches!(
                message,
                DiscoveryMessage::HostFinished {
                    completion: DiscoveryCompletion::Malformed,
                    ..
                }
            )),
            "case {index} did not report malformed: {messages:?}"
        );
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

#[cfg(unix)]
#[test]
fn remote_entry_limit_stops_directory_enumeration_incrementally() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("child"), "").unwrap();
    let find = temp.path().join("find");
    fs::write(
        &find,
        "#!/bin/sh\nwhile :; do printf '%s\\n' \"$1/child\"; done\n",
    )
    .unwrap();
    fs::set_permissions(&find, fs::Permissions::from_mode(0o700)).unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nPATH='{}':$PATH\nexport PATH\nfor argument do command=$argument; done\nexec /bin/sh -c \"$command\"\n",
            temp.path().display()
        ),
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
            generation: 6,
            locations: vec![DiscoveryLocation {
                host: "dev".into(),
                target: Some("dev.example".into()),
                roots: vec![root.to_string_lossy().into_owned()],
            }],
        },
    );

    assert!(
        messages.iter().any(|message| matches!(
            message,
            DiscoveryMessage::HostFinished {
                completion: DiscoveryCompletion::EntriesLimit,
                ..
            }
        )),
        "remote enumeration did not stop at the entry bound: {messages:?}"
    );
}

#[test]
fn result_budget_is_global_and_truncation_preserves_request_and_lexical_order() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    repo(&first.join("a"), false);
    repo(&first.join("b"), false);
    repo(&second.join("c"), false);
    repo(&second.join("d"), false);
    let mut bounded = limits();
    bounded.max_results = 2;
    bounded.workers = 2;
    let service = DiscoveryService::new(
        ProcessBinaries::new(temp.path().join("ssh"), temp.path().join("tmux")),
        bounded,
    );
    let run = service.start(DiscoveryRequest {
        generation: 30,
        locations: vec![
            DiscoveryLocation {
                host: "first".into(),
                target: None,
                roots: vec![first.to_string_lossy().into_owned()],
            },
            DiscoveryLocation {
                host: "second".into(),
                target: None,
                roots: vec![second.to_string_lossy().into_owned()],
            },
        ],
    });
    let messages = collect_messages(&run, 30);

    assert_eq!(
        messages
            .iter()
            .filter_map(|message| match message {
                DiscoveryMessage::Repository { host, path, .. } => {
                    Some((
                        host.as_str(),
                        Path::new(path).file_name().unwrap().to_str().unwrap(),
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![("first", "a"), ("first", "b")],
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            host,
            completion: DiscoveryCompletion::ResultsLimit,
            ..
        } if host == "second"
    )));
    assert_eq!(run.completion(), Some(DiscoveryRunCompletion::ResultsLimit));
}

#[test]
fn all_locations_share_one_injected_request_deadline() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    repo(&first.join("a"), false);
    repo(&second.join("b"), false);
    let base = Instant::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let clock_calls = Arc::clone(&calls);
    let service = DiscoveryService::new_with_clock(
        ProcessBinaries::new(temp.path().join("ssh"), temp.path().join("tmux")),
        limits(),
        move || {
            if clock_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                base
            } else {
                base + Duration::from_secs(2)
            }
        },
    );
    let run = service.start(DiscoveryRequest {
        generation: 31,
        locations: vec![
            DiscoveryLocation {
                host: "first".into(),
                target: None,
                roots: vec![first.to_string_lossy().into_owned()],
            },
            DiscoveryLocation {
                host: "second".into(),
                target: None,
                roots: vec![second.to_string_lossy().into_owned()],
            },
        ],
    });
    let messages = collect_messages(&run, 31);

    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(
                message,
                DiscoveryMessage::HostFinished {
                    completion: DiscoveryCompletion::TimedOut,
                    ..
                }
            ))
            .count(),
        2,
    );
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, DiscoveryMessage::Repository { .. }))
    );
    assert_eq!(run.completion(), Some(DiscoveryRunCompletion::TimedOut));
}

#[cfg(unix)]
#[test]
fn cancellation_stops_queued_fanout_and_reports_cancelled_completion() {
    let temp = tempdir().unwrap();
    let calls = temp.path().join("calls");
    let ssh = temp.path().join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\nwhile :; do :; done\n",
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bounded = limits();
    bounded.workers = 1;
    bounded.timeout = Duration::from_secs(2);
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        bounded,
    );
    let run = service.start(DiscoveryRequest {
        generation: 32,
        locations: (0..8)
            .map(|index| DiscoveryLocation {
                host: format!("host-{index}"),
                target: Some(format!("host-{index}.example")),
                roots: vec!["/work".into()],
            })
            .collect(),
    });
    let marker_deadline = Instant::now() + Duration::from_secs(1);
    while !calls.exists() {
        assert!(
            Instant::now() < marker_deadline,
            "injected probe never started"
        );
        std::thread::yield_now();
    }
    run.cancel();
    loop {
        if matches!(
            run.receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            DiscoveryMessage::Finished { generation: 32 }
        ) {
            break;
        }
    }

    assert_eq!(fs::read_to_string(calls).unwrap(), "x");
    assert_eq!(run.completion(), Some(DiscoveryRunCompletion::Cancelled));
}

#[test]
fn excessive_locations_and_roots_are_rejected_before_fanout() {
    let temp = tempdir().unwrap();
    let marker = temp.path().join("ssh-was-started");
    let ssh = temp.path().join("ssh");
    #[cfg(unix)]
    {
        fs::write(&ssh, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        limits(),
    );

    let excessive_locations = service.start(DiscoveryRequest {
        generation: 40,
        locations: (0..257)
            .map(|index| DiscoveryLocation {
                host: format!("host-{index:03}"),
                target: Some(format!("host-{index:03}.example")),
                roots: Vec::new(),
            })
            .collect(),
    });
    assert_eq!(
        collect_messages(&excessive_locations, 40),
        vec![DiscoveryMessage::Finished { generation: 40 }]
    );
    assert_eq!(
        excessive_locations.completion(),
        Some(DiscoveryRunCompletion::Error)
    );

    let excessive_roots = service.start(DiscoveryRequest {
        generation: 41,
        locations: vec![DiscoveryLocation {
            host: "host".into(),
            target: Some("host.example".into()),
            roots: (0..257).map(|index| format!("/root-{index:03}")).collect(),
        }],
    });
    assert_eq!(
        collect_messages(&excessive_roots, 41),
        vec![DiscoveryMessage::Finished { generation: 41 }]
    );
    assert_eq!(
        excessive_roots.completion(),
        Some(DiscoveryRunCompletion::Error)
    );
    assert!(!marker.exists(), "invalid requests reached remote fanout");
}

#[cfg(unix)]
#[test]
fn slow_first_remote_stream_reserves_the_global_result_cap_in_request_order() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let release = temp.path().join("release-first");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *first.example*) while [ ! -e '{}' ]; do :; done ;;\n\
             *) : > '{}' ;;\n\
             esac\n\
             printf 'R\\0000\\000a\\000R\\0000\\000b\\000R\\0000\\000c\\000R\\0000\\000d\\000'\n",
            release.display(),
            release.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bounded = limits();
    bounded.max_results = 3;
    bounded.workers = 2;
    bounded.timeout = Duration::from_secs(10);
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        bounded,
    );
    let run = service.start(DiscoveryRequest {
        generation: 42,
        locations: vec![
            DiscoveryLocation {
                host: "first".into(),
                target: Some("first.example".into()),
                roots: vec!["/work".into()],
            },
            DiscoveryLocation {
                host: "second".into(),
                target: Some("second.example".into()),
                roots: vec!["/work".into()],
            },
        ],
    });
    let messages = collect_messages(&run, 42);

    assert_eq!(
        messages
            .iter()
            .filter_map(|message| match message {
                DiscoveryMessage::Repository { host, path, .. } => {
                    Some((host.as_str(), path.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            ("first", "/work/a"),
            ("first", "/work/b"),
            ("first", "/work/c"),
        ]
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        DiscoveryMessage::HostFinished {
            host,
            completion: DiscoveryCompletion::ResultsLimit,
            ..
        } if host == "first"
    )));
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, DiscoveryMessage::Finished { .. }))
            .count(),
        1
    );
    assert_eq!(run.completion(), Some(DiscoveryRunCompletion::ResultsLimit));
}

#[cfg(unix)]
#[test]
fn remote_entry_allowances_are_request_ordered_and_request_wide() {
    let temp = tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    let log = temp.path().join("entry-limits");
    let release = temp.path().join("release-first");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
             previous=\n\
             for argument do\n\
                 target=$previous\n\
                 command=$argument\n\
                 previous=$argument\n\
             done\n\
             eval \"set -- $command\"\n\
             printf '%s %s\\n' \"$target\" \"$7\" >> '{}'\n\
             case \"$target\" in\n\
                 first.example)\n\
                     while [ ! -e '{}' ]; do :; done\n\
                     ;;\n\
                 second.example)\n\
                     : > '{}'\n\
                     printf 'N\\000'\n\
                     ;;\n\
                 *) printf 'N\\000' ;;\n\
             esac\n",
            log.display(),
            release.display(),
            release.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bounded = limits();
    bounded.max_entries = 4;
    bounded.workers = 4;
    bounded.timeout = Duration::from_secs(10);
    let service = DiscoveryService::new(
        ProcessBinaries::new(&ssh, temp.path().join("tmux")),
        bounded,
    );
    let locations = (1..=6)
        .map(|index| DiscoveryLocation {
            host: format!("host-{index}"),
            target: Some(match index {
                1 => "first.example".into(),
                2 => "second.example".into(),
                _ => format!("remote-{index}.example"),
            }),
            roots: vec!["/work".into()],
        })
        .collect::<Vec<_>>();

    for generation in 50..53 {
        let _ = fs::remove_file(&log);
        let _ = fs::remove_file(&release);
        let run = service.start(DiscoveryRequest {
            generation,
            locations: locations.clone(),
        });
        let messages = collect_messages(&run, generation);
        let mut allowances = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| {
                let (target, allowance) = line.rsplit_once(' ').unwrap();
                (target.to_owned(), allowance.parse::<usize>().unwrap())
            })
            .collect::<Vec<_>>();
        allowances.sort();

        assert_eq!(
            allowances,
            vec![
                ("first.example".into(), 1),
                ("remote-3.example".into(), 1),
                ("remote-4.example".into(), 1),
                ("second.example".into(), 1),
            ]
        );
        assert!(allowances.iter().map(|(_, limit)| limit).sum::<usize>() <= 4);
        assert_eq!(
            messages,
            (1..=6)
                .map(|index| DiscoveryMessage::HostFinished {
                    generation,
                    host: format!("host-{index}"),
                    completion: if index == 1 {
                        DiscoveryCompletion::Complete
                    } else {
                        DiscoveryCompletion::EntriesLimit
                    },
                })
                .chain(std::iter::once(DiscoveryMessage::Finished { generation }))
                .collect::<Vec<_>>()
        );
        assert_eq!(run.completion(), Some(DiscoveryRunCompletion::EntriesLimit));
    }
}
