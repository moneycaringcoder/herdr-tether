# Changelog

## [Unreleased]

## [0.2.0] - 2026-07-11

### Added

- Reworked the Herdr explorer around a host-to-resource flow with progressive reachability, owned-workload status, safe external `tmux` catalogs, bounded repository discovery, explicit refresh, filtering, direct paths, and retained owned-only groups for removed or retargeted hosts.
- Added visible Active, Closing, and Closed lifecycle states. Active work can resume or close, Closing work can retry exact close, and Closed metadata can be pruned only after an explicit preview and confirmation; destructive actions remain unavailable for external sessions.
- Added exact, non-owning attachment to validated external `tmux` sessions. Tether never adopts, persists, renames, closes, kills, marks, or prunes them.
- Added deterministic `snapshot [--pretty]` schema version 1 for hosts, effective roots, repositories, complete owned metadata and live Active status, and safe external catalogs. Typed partial states preserve degraded results without exposing preset commands, child output, raw backend errors, or storage paths.
- Added version-2 configuration for bounded discovery roots/limits and Closed-record retention. Version-0/version-1 config and version-0 state migrate under the normal advisory lock; recent directories remain suggestions rather than silently widening scan scope.
- Added stronger setup and doctor guidance for configuration, state, `tmux`, SSH, Cargo build prerequisites, and Herdr integration.

### Changed

- Plugin integration is validated against Herdr 0.7.3, the declared minimum. Plugin actions open a managed terminal overlay, then create and focus the selected split-right, split-down, or new-tab placement using the invoking pane context.
- Status, discovery, close, and snapshot collection use bounded concurrent workers with deadlines, capped output, cancellation, and child-process cleanup so slow targets do not block completed results.
- Owned state is grouped by its persisted host and target. Removed or retargeted groups remain visible for lifecycle recovery but are never probed or scanned through changed configuration.

### Fixed

- Corrected installed-plugin executable resolution, Herdr 0.7.3 response handling, invoking-pane placement, plugin config/state forwarding, recursive placement prevention, and new-tab focus.
- Exact close now reconciles concurrent and failed operations through recoverable Closing state without holding persistence locks during transport.

### Limitations

- Remote workloads remain SSH-backed `tmux`; Tether does not provide native remote Herdr federation or pane streaming.
- Herdr 0.7.3 exposes plugin interaction through a terminal overlay rather than a native sidebar or non-terminal plugin panel.
- External sessions remain intentionally outside Tether ownership and lifecycle management.

## [0.1.0] - 2026-07-10

### Added

- macOS and Linux CLI for setup, host catalog/checks, session open/list/resume/close/prune, and installation diagnostics.
- Durable local and remote `tmux` sessions; remote execution uses ordinary OpenSSH connections and the user's authentication and strict host-key policy.
- Explicit close semantics that retire workloads proven missing, kill only exact running sessions, and leave indeterminate workloads unchanged; conservative prune removes only old metadata for explicitly closed sessions and does not reconnect to or probe hosts.
- Advisory-locked configuration and state transactions with private Unix directory/file permissions; recoverable `closing` markers prevent kill/save partial failures from masquerading as active workloads; no credential storage or telemetry.
- Herdr plugin picker/setup overlays and resume placement in a right split, down split, or new tab.
- JSON output for scriptable host and session listings.

### Limitations

- Remote sessions are SSH-backed `tmux`, not native remote Herdr federation or remote pane streaming.

### Acknowledgement

- `herdr-mirror` 0.1.6 (MIT) was inspected as a design influence only; no topology or code was copied.
