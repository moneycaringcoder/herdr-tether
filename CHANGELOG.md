# Changelog

## [Unreleased]

### Added

- The managed terminal overlay now presents one stable resource stage per effective host: exact-target Active, Closing, and Closed Tether metadata, safely discovered external tmux sessions, then **Create new Tether workload**. Removed or retargeted metadata remains visible in explicit retained owned-only groups. Active resume and external attach both use the existing focused split-right, split-down, and new-tab placement boundary.
- Added exact non-owning attachment for validated external names. External sessions are visibly labeled and never adopted, persisted, renamed, closed, killed, or pruned; reserved `tether-*` collisions are hidden and non-actionable.
- Added progressive, non-destructive host and Tether workload status to the explorer. Four bounded workers isolate slow hosts; each local/SSH probe has a three-second deadline, cancellation and process-group cleanup, null stdin, and capped output.
- Added explicit `r` refresh with generation-safe stale/fresh presentation. Host rows distinguish online/offline/timeout/error; workload rows distinguish running/missing/unknown/timeout/error, and freshly missing workloads cannot be resumed.
- Expanded the bounded per-host status probe into one validated tmux catalog snapshot that derives both owned workload state and external rows. Duplicate/structurally malformed catalogs fail closed; refresh keeps prior external rows visibly stale and selected by stable identity.
- Added asynchronous bounded repository discovery beneath configured per-host roots plus the local `HOME`/remote `~` fallback. Recent directories remain picker suggestions without widening scan scope. Local traversal is deterministic and does not follow symlinks; remote scans use BatchMode SSH, quoted root argv, configurable validated depth/entry/result/deadline/worker bounds, a fixed output cap, and a validated NUL-framed protocol.
- Added case-insensitive directory filtering with `/` and literal direct-path entry with `p`. Refresh now restarts both status and discovery generations while rejecting stale results.
- Added version-2 TOML discovery roots/limits and retention defaults. Existing v0/v1 config and v0 state now migrate under the same advisory lock as normal transactions; recent picker directories no longer silently widen repository scan scope.
- Expanded idempotent setup with effective-default/prerequisite/next-step guidance, and made doctor classify required config/state/tmux/SSH/Cargo/Herdr failures with bounded deadlines/output while continuing the complete report. Explicit prune age continues to override configured retention.
- Added owned-only explorer close: press-only `c` opens a red exact-ID confirmation on Active or Closing rows, `y` starts a bounded asynchronous close that cannot be abandoned through picker exit, and Closed/external/Create rows cannot enter the destructive type. Completion rereads the exact persisted record without transport; success retains Closed metadata, while failure reconciles authoritative Active/Closing state with sanitized in-place recovery. CLI and explorer share a lock-minimizing two-phase lifecycle with three-second transport deadlines, process-group cleanup, idempotent peer finalization, and recoverable `closing` semantics.
- Added global explorer metadata cleanup: press-only uppercase `P` asynchronously previews eligible Closed records, then red count/retention guidance and explicit `y` apply an immutable exact-record preview without host contact or catalog refresh. Read-only preview remains cancellable; confirmed apply is non-abandonable, atomic, retryable, and reports concurrent skips. CLI and explorer now share a state-only prune service that cannot widen to non-preview, changed, active, closing, or external records.
- Added `snapshot [--pretty]`, a schema-version-1 deterministic JSON view of explorer hosts, effective roots, repositories, complete owned metadata/live Active status, and safe external catalogs. It reuses concurrent bounded status/discovery services, reports degradation and retained state-only targets as typed partial data, preserves legacy JSON shapes, drains cancelled probes before return, exposes no preset commands or child output, and has no lifecycle or persistence-mutation capability.
- Added exact current/retained explorer inventory: state is grouped by persisted `(host,target)`, current targets keep status/catalog/discovery/create capabilities, and removed or retargeted groups expose owned lifecycle metadata only. Retained targets and non-Active records are never background-probed or scanned. Active rows resume and close, Closing rows retry close, Closed rows are prune-only, close results reconcile exact authoritative state, and prune apply removes only confirmed IDs reported removed while preserving concurrent skips.
- Documented three end-to-end workflows—repository discovery through durable resume, multi-host status without attachment, and safe external tmux attach—with an explicit comparison to manual SSH/tmux and herdr-mirror.

### Fixed

- Fixed installed Herdr 0.7.3 action/placement interoperability: managed pane commands now resolve the built executable through `HERDR_PLUGIN_ROOT`; split responses use the actual `pane_info` contract; successful `pane run` accepts Herdr's empty stdout; placement targets the invoking pane rather than the temporary overlay; placed attach commands retain authoritative plugin config/state paths without recursively opening another pane; and new tabs are explicitly focused.

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
