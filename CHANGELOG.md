# Changelog

## [Unreleased]

### Added

- Added opt-in orchestration groups in development state schema version 4, with harness-neutral orchestrator/worker references, persisted membership epochs, explicit output-observation and interactive-open capabilities, and metadata-only membership lifecycle commands. Development schema version 3 migrates forward with fresh epochs.
- Added a read-only Observer companion that uses one outer Herdr pane, dynamically refreshes exact-owned worker status and authorized bounded captures, rejects stale asynchronous results against complete authorization fingerprints, restores terminal state defensively, renders deterministic four-worker pages, and can open an authorized running worker without replacing Observer.
- Added development `main` and immutable exact-SHA operating guidance for groups and Observer while retaining stable v0.3.0 installation instructions.
- Added a native `prefix+t` Observer manager that creates safe group metadata from running workloads, edits membership reversibly, confirms metadata-only deletion, and launches one companion pane from the current Herdr plugin context without exposing session IDs or requiring standalone CLI setup.

### Fixed

- Debounced queued or repeated `Enter` events inside Observer so one open gesture places one interactive view, reports progress, and does not change ordinary intentional multi-attach behavior.
- Replaced collapsed tile borders at tiny pane sizes with a bounded resize message and made Observer chrome use terminal-default foreground and background colors while retaining sanitized capture text.
- Kept companion creation from running in or closing its source launcher pane, including when configured placement requests replacement.

### Limitations

- Observer launch requires an invoking Herdr pane context; processes nested inside Tether workloads must request or hand off that explicit launch rather than assuming Herdr's plugin environment.
- Observer is bounded to 32 groups, 64 workers per group, four captures per page, and 200 lines, 16 KiB, and 16,384 display cells per rendered capture.
- Companion Observer launches normalize `replace-current-pane` to `split-right` so the invoking or Observer pane remains available.

## [0.3.0] - 2026-07-11

### Added

- Added an ended-workload lifecycle with native pane-exit observation, retained exit context, safe Restart in the original directory, and metadata-only Remove.
- Added automatic cleanup of safely finalized history after the configured retention period; cleanup has no SSH, `tmux`, probe, or Stop capability.
- Added focused quickstart, lifecycle, configuration, troubleshooting, architecture/security, and contribution paths plus original light/dark-ready project artwork.
- Added an optional first-party Hermes skill for observable, configurable orchestration through Tether's public snapshot and lifecycle interfaces.

### Changed

- Simplified the picker around contextual `Enter Open` / `x Stop`, `Enter Restart` / `x Remove`, and `r Retry` actions, with history maintenance moved out of the primary workflow.
- Stop, Restart, Remove, and migration transitions now preserve recoverable identity and state across persistence, transport, placement, and concurrent-operation failures.
- Public installation, lifecycle, security, and limitations guidance now uses a concise first-visit path and evergreen support wording.
- Normal picker, snapshot, and session-list views now hide Removed tombstones, group active/transitional workloads ahead of ended workloads, and retain recent-first deterministic ordering within each group.
- Public installation guidance now distinguishes stable v0.3.0, development `main`, and immutable exact-SHA installs; Herdr marketplace metadata now describes the durable-workload benefit directly.

### Fixed

- Workloads whose commands exit are no longer left as contradictory active/missing rows or offered as failed attachments.
- Status and discovery refreshes no longer dismiss or route input around active confirmation and error dialogs.
- Preserved remote `~` expansion and captured-internal-identity rollback while extending lifecycle behavior.
- Herdr panes now receive concise bounded titles from safe host, directory, and command metadata instead of a generic session label.
- Corrected the README mark's cursor/tether alignment across all public logo assets and removed an unsupported Herdr build-manifest field.

### Limitations

- Remote workloads remain SSH-backed `tmux`; Tether does not provide native remote Herdr federation or pane streaming.
- Herdr 0.7.3 has no general nested-workload registration API. Tether uses clear pane/session titles and does not fabricate sidebar agent state.

## [0.2.2] - 2026-07-11

### Fixed

- Remote default directories using `~` or `~/...` now expand against the remote login environment before launch and cwd verification.
- Failed post-create verification now rolls back through the captured internal tmux session identity instead of racing a second name lookup.

## [0.2.1] - 2026-07-11

### Added

- Added explicit `setup keybinding` and `--rollback` commands for an idempotent, backed-up `prefix+t` binding with conflict refusal, merged-TOML validation, later-edit protection, one-shot rollback that consumes its backup, and Herdr config reload.
- Added Replace current pane with foreground-process confirmation, destination-first readiness checks, source preservation on pre-close failure, and explicit destination cleanup reporting.

### Changed

- Tether now resolves executables for reduced-path GUI/Homebrew launches, verifies the exact created tmux cwd, enables mouse only for owned sessions, and keeps failed explorer operations stable until Enter retries or Backspace/Esc cancels.
- Workloads retain their login-shell command environment, then explicitly restore and inode-verify the selected local/remote directory before success.

### Limitations

- Tether requires tmux 3.3 or newer locally and on remote targets.
- Herdr 0.7.3 requires one explicit **Install prefix+t launcher** action because it has no generic plugin-action menu. Normal plugin install/open and plain standalone setup remain non-mutating.
- Nested tools behind SSH and `tmux` may not appear in Herdr's Agents sidebar. Tether uses titled panes and does not fabricate native agent state.

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
- Herdr 0.7.3 provides semantic agent-report APIs, but Tether cannot infer a nested agent lifecycle or native session identity from an outer tmux session; titled Tether panes remain the honest fallback.
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
