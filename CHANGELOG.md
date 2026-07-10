# Changelog

## [0.1.0] - Unreleased

### Added

- macOS and Linux CLI for setup, host catalog/checks, session open/list/resume/close/prune, and installation diagnostics.
- Durable local and remote `tmux` sessions; remote execution uses ordinary OpenSSH connections and the user's authentication and strict host-key policy.
- Explicit close semantics that retire workloads proven missing, kill only exact running sessions, and leave indeterminate workloads unchanged; conservative prune removes only old metadata for explicitly closed sessions and does not reconnect to or probe hosts.
- Atomic configuration and state persistence with private Unix directory and file permissions; no credential storage or telemetry.
- Herdr plugin picker/setup overlays and resume placement in a right split, down split, or new tab.
- JSON output for scriptable host and session listings.

### Limitations and verification

- Remote sessions are SSH-backed `tmux`, not native remote Herdr federation or remote pane streaming.
- Automated coverage passed 35 tests and a locked release build. Herdr 0.7.3 plugin link, action listing, and unlink were exercised live.
- A local `tmux` session survived a failed non-TTY attach; explicit close then removed only that session.
- Live SSH localhost lifecycle verification is incomplete because strict host-key verification rejected the attempted connection. No successful live remote lifecycle is claimed.

### Acknowledgement

- `herdr-mirror` 0.1.6 (MIT) was inspected as a design influence only; no topology or code was copied.
