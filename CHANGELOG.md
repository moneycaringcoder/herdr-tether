# Changelog

## [Unreleased]

### Added

- Tether now warns when Herdr does not recognize an explicit `--herdr-agent` or
  `herdr_agent` kind, listing the kinds the running Herdr does recognize. The
  warning is advisory and never blocks creation, because Herdr updates its agent
  manifests remotely and independently of Tether.

### Fixed

- Corrected the documented agent-kind examples. Herdr recognizes `claude`, not
  `claude-code`; the previous example was a kind Herdr does not know, so anyone
  following it got no sidebar agent row and no explanation.

## [0.6.0] - 2026-08-04

### Added

- Added opt-in Herdr toasts when a Mission Control agent changes into `BLOCKED`
  or `DONE`, configured under `[notifications]` and on by default. A toast is
  sent only on a change into that state, never repeatedly while an agent stays
  there. Delivery is advisory: Herdr shows nothing unless its own
  `ui.toast.delivery` is enabled, and Tether treats a refused toast as normal
  because the Mission Control tile remains the authoritative view. Notification
  text carries only the sanitized worker label and its state.
- Mission Control now marks a capture `TRUNCATED` when Herdr reports that older
  output was dropped. Tether previously discarded that flag and rendered a
  clipped capture as if it were the worker's complete output.
- Added `e` in Mission Control to relay Herdr's own `agent.explain` reasoning for
  the selected agent, which is most useful on a `BLOCKED` or `UNKNOWN` tile.
  Herdr's explain payload is an open object, so Tether flattens only top-level
  scalars into bounded, sanitized pairs, reports collections by shape, and
  surfaces unrecognized fields rather than dropping them.
- Documented `grok`, `agy`, and `opencode` as `--herdr-agent` examples. The kind
  stays a free-form token that Herdr resolves, so agents Herdr adds later need no
  Tether release.

### Changed

- **Breaking:** Tether now requires Herdr 0.8.0 or newer. `min_herdr_version` is
  `0.8.0`, so Herdr refuses to install or link Tether on an older release. Stay
  on Tether v0.5.1 until Herdr is upgraded.
- Replaced the 0.7.3/0.7.4/0.7.5 capability ladder with one code path. Tether now
  validates the Herdr socket API protocol reported by `session.snapshot` instead
  of comparing version strings, so a build whose version looks new but whose
  protocol is old is rejected with an explicit upgrade message.
- Managed picker and setup surfaces declare `placement = "popup"` with explicit
  sizing directly in `herdr-plugin.toml`, removing two `herdr --version`
  subprocess probes from the pane-open and Agent-view metadata paths.
- Mission Control availability now reflects Herdr reachability rather than Herdr
  version; the group action screen and prompt-permission notices say so.
- Live-product coverage now exercises Herdr 0.8.0 on Ubuntu 24.04 and macOS 14
  instead of Herdr 0.7.3 and 0.7.5.

### Fixed

- Restored Mission Control's event-driven agent status on Herdr 0.8.0. Herdr
  requires a `pane_id` on `pane.agent_status_changed`, and an unfiltered entry
  invalidates the whole `events.subscribe` request rather than just that entry,
  so Tether was receiving no events at all and silently falling back to its
  one-second refresh. Pane lifecycle events stay global; agent status is now
  subscribed per bound group pane and re-established when that set changes.
- Corrected public Herdr links after the upstream GitHub organization migration
  from `ogulcancelik/herdr` to `herdrdev/herdr`.
- Repaired the macOS live-product gate, which reported success without running
  the smoke at all. Bash 3.2 treats an empty `"${array[@]}"` as unbound under
  `set -u`, and exits with the status of the last command in an `EXIT` trap, so
  the abort was masked. The expansion is now empty-safe and the trap re-raises
  the real status.
- A stopped Herdr server, a protocol mismatch, or a missing API now reports
  actionable guidance instead of a raw socket I/O error.
- Restored private `0700` permissions on storage directories Tether creates
  under a symlinked or GNU Stow ancestor; v0.5.1 left those directories at the
  process umask. Directories that already exist behind a link continue to keep
  their own permissions.
- Added path context to Herdr keybinding permission-preservation failures, so a
  non-regular config or backup target reports which file was rejected.

## [0.5.1] - 2026-07-28

### Fixed

- Added consistent GNU Stow and symlink support for Tether configuration, state,
  Agent view preferences, and Herdr keybinding backup/rollback while preserving
  atomic writes and rejecting dangling or non-regular targets. Thanks to
  [@lkshrk](https://github.com/lkshrk) for the first community contribution in
  [#34](https://github.com/moneycaringcoder/herdr-tether/pull/34).

## [0.5.0] - 2026-07-24

### Added

- Added Herdr 0.7.5+ Mission Control: event-driven agent states, exact metadata-bound group-member resolution, bounded multi-target reviewed prompt-and-wait delivery, focus/read/wait actions, reconnect resnapshots, and explicit stale-state recovery without recurring SSH capture for attached agents.
- Added the fail-closed `prompt_agent` worker capability, defaulting to false
  in existing schema-v4 state, with native reviewed manager grants and the
  optional `orchestration add-worker --prompt-agent` adapter. Recognized
  read-only agents still use event-driven status and their separately granted
  read/focus actions without inheriting prompt authority.
- Added bounded OpenSSH `Include` alias discovery with cycle, file, byte, and
  directory-containment guards; remote operations now reuse OpenSSH control
  connections and report successful observation latency in Mission Control.
- Added native Agents sidebar presets for the full group, agents needing
  attention, and remote group agents, backed by source-owned group and
  remote-origin metadata tokens.

### Security

- Revalidate membership epoch, Tether ownership, running state, explicit agent kind, exact current Herdr occupant, and settled state before every prompt; prompt text remains memory-only, ambiguous or replaced panes receive no input, partial fan-out is explicit, and uncertain delivery is never retried automatically.
- Keep Herdr 0.7.3/0.7.4 workload management and Observer behavior intact while exposing active agent controls only on Herdr 0.7.5+.


## [0.4.1] - 2026-07-23

### Fixed

- Corrected stable documentation that still described v0.4.0 orchestration, picker context, and Hermes workflows as development-only.
- Documented versioned Herdr agent recognition, native Agents sidebar setup and recovery, the private `agent-view.json` preference, and the presentation-only Agent metadata security boundary.
- Updated quickstart, troubleshooting, architecture, contributor, visual-asset, and Hermes guidance to match the released popup, agent-hint, group-view, package, and CI behavior.

## [0.4.0] - 2026-07-23

### Added

- Added opt-in orchestration groups in development state schema version 4, with harness-neutral orchestrator/worker references, persisted membership epochs, explicit output-observation and interactive-open capabilities, and metadata-only membership lifecycle commands. Development schema version 3 migrates forward with fresh epochs.
- Added a read-only Observer companion that uses one outer Herdr pane, dynamically refreshes exact-owned worker status and authorized bounded captures, rejects stale asynchronous results against complete authorization fingerprints, restores terminal state defensively, renders deterministic four-worker pages, and can open an authorized running worker without replacing Observer.
- Added stable v0.4.0, development `main`, and immutable exact-SHA operating guidance for groups and Observer.
- Added a native `prefix+t` Observer manager that creates safe group metadata from running workloads, edits membership reversibly, confirms metadata-only deletion, and launches one companion pane from the current Herdr plugin context without exposing session IDs or requiring standalone CLI setup.
- Added explicit topology review with visible `ORCHESTRATOR` and `WORKER` roles, group health summaries, and metadata-only orchestrator reassignment that preserves unaffected worker membership.
- Added stable bounded reference tokens only when otherwise-safe workload or Observer tile labels are ambiguous, while retaining exact internal identity for every action.
- Added distinct Observer capture states for loading, ready-but-empty output, and unavailable output; empty groups now expose only refresh and back actions.
- Added bounded picker and Observer-manager viewport position and overflow guidance, plus explicit resize fallbacks for panes too small to show actionable content.
- Added finite persisted-state, configuration, discovery-request, and status-request budgets with deterministic boundary rejection, global discovery completion reasons, and exact-target status deduplication.
- Added versioned, bounded, redacted JSON output for doctor diagnostics and live product-smoke evidence.
- Added deterministic public documentation, canonical default/version, package-derived hygiene, installed-artifact runtime, archive-budget, and documented CLI contract checks.
- Added read-only setup preflight for required runtime tools before any Tether or Herdr file mutation.
- Added optional documented Herdr pane/workspace context prioritization that reorders only known, unambiguous picker entries.
- Added correlation-safe redaction for plugin action and entrypoint failures, plus official-Herdr keyboard-only picker checks at 80×24 and 48×14.

- Added explicit validated Herdr agent-kind hints for presets and ad hoc workload creation; hinted local and SSH-backed attachments expose `HERDR_AGENT=<kind>` so Herdr 0.7.5 can recognize agents hidden behind `tmux` without Tether guessing from commands.
- Added opt-in, source-owned native Agent sidebar views for orchestration groups on Herdr 0.7.5, including persisted startup/live-handoff restoration, reversible default-view recovery, and group tokens on newly opened member panes.
- Added Herdr 0.7.5 live-product coverage while retaining Herdr 0.7.3 compatibility, and switched managed picker/setup panes to Herdr's session-modal popup when the installed version supports it.

### Fixed

- Debounced queued or repeated `Enter` events inside Observer so one open gesture places one interactive view, reports progress, and does not change ordinary intentional multi-attach behavior.
- Replaced collapsed tile borders at tiny pane sizes with a bounded resize message and made Observer chrome use terminal-default foreground and background colors while retaining sanitized capture text.
- Kept companion creation from running in or closing its source launcher pane, including when configured placement requests replacement.
- Revalidated every newly admitted orchestrator or worker as a running exact-owned workload inside the locked state transaction, including optional CLI adapter operations, while preserving already-retained unavailable members.
- Rejected manager edit or delete requests when the displayed group snapshot changed before commit, preventing stale screens from overwriting or deleting concurrent metadata updates.
- Required native Observer-manager placement to use `HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id` and fail closed instead of falling back to a managed overlay pane ID.
- Kept the last authorized Observer tiles, page, selection, and warning visible across recoverable refresh failures until a successful retry, while mapping capture failures to unavailable instead of rendering error text as output.
- Made held navigation repeat-safe while keeping refresh, open, and quit single-shot, and made capture sanitization and truncation preserve extended grapheme clusters such as flags, keycaps, combining sequences, and emoji.
- Reconciled orchestration membership atomically when workload metadata is removed, and made post-kill finalization retryable without sending a second backend kill.
- Preserved existing regular-file permission bits across atomic configuration and state replacement while keeping newly created private files at mode `0600`.

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
