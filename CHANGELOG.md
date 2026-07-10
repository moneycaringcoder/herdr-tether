# Changelog

## [Unreleased]

### Added

- The managed terminal overlay now lists active Tether workloads beneath each host, resumes the selected exact session ID without traversing creation fields, and retains **Create new workload** as a sibling action. Hosts without active workloads proceed directly to creation.
- Explorer resume and create intents share the existing focused split-right, split-down, and new-tab placement boundary. Closing and closed records are excluded from resumable choices.
- Added progressive, non-destructive host and Tether workload status to the explorer. Four bounded workers isolate slow hosts; each local/SSH probe has a three-second deadline, cancellation and process-group cleanup, null stdin, and capped output.
- Added explicit `r` refresh with generation-safe stale/fresh presentation. Host rows distinguish online/offline/timeout/error; workload rows distinguish running/missing/unknown/timeout/error, and freshly missing workloads cannot be resumed.
- Added asynchronous bounded repository discovery beneath recent workload directories, configured roots, and the local `HOME`/remote `~` fallback. Local traversal is deterministic and does not follow symlinks; remote scans use BatchMode SSH, quoted root argv, fixed depth/entry/result/deadline/output limits, and a validated NUL-framed protocol.
- Added case-insensitive directory filtering with `/` and literal direct-path entry with `p`. Refresh now restarts both status and discovery generations while rejecting stale results.

## [0.1.0] - 2026-07-10

### Added

- macOS and Linux CLI for setup, host catalog/checks, session open/list/resume/close/prune, and installation diagnostics.
- Durable local and remote `tmux` sessions; remote execution uses ordinary OpenSSH connections and the user's authentication and strict host-key policy.
- Explicit close semantics that retire workloads proven missing, kill only exact running sessions, and leave indeterminate workloads unchanged; conservative prune removes only old metadata for explicitly closed sessions and does not reconnect to or probe hosts.
- Advisory-locked configuration and state transactions with private Unix directory/file permissions; recoverable `closing` markers prevent kill/save partial failures from masquerading as active workloads; no credential storage or telemetry.
- Herdr plugin picker/setup overlays and resume placement in a right split, down split, or new tab.
- JSON output for scriptable host and session listings.

### Limitations and verification

- Remote sessions are SSH-backed `tmux`, not native remote Herdr federation or remote pane streaming.
- Automated coverage passed 36 tests and a locked release build. Herdr 0.7.3 plugin link, action listing, and unlink were exercised live; GitHub CI passed on Ubuntu 24.04 and macOS 14.
- A strict-BatchMode live SSH run from Hermes to `dev` verified remote create, real-TTY attach, detach, same-PID counter continuity, resume, exact close, and prune isolation.
- Live adversarial quoting preserved a directory containing spaces and literal shell metacharacters without creating the injected sentinel. Native Herdr placement interaction and the macOS live lifecycle remain acceptance checks.

### Acknowledgement

- `herdr-mirror` 0.1.6 (MIT) was inspected as a design influence only; no topology or code was copied.
