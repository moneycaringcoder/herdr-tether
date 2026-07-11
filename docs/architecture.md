# Tether architecture

## Purpose and invariants

Tether makes the lifetime of a terminal workload independent of the Herdr pane or SSH connection viewing it. In v0.1 the durable unit is one exactly named `tmux` session. A pane is an attach client, not the workload owner.

The implementation preserves these invariants:

1. creating a workload precedes recording it as active;
2. a persistence failure after create triggers a best-effort close; if rollback also fails, the exact workload ID and both failures are reported;
3. attach/resume never closes a workload, including when attach fails or the connection drops;
4. only explicit close may invoke `kill-session`; before killing a running workload it persists a recoverable `closing` marker, then finalizes metadata only after the workload is proven missing or exact-session kill succeeds;
5. config and state mutations hold per-file advisory locks across load, mutation, and atomic save;
6. pruning removes only old closed metadata and never kills or probes workloads;
7. every attachment uses an exact tmux target derived from either an owned `SessionId` or a validated non-Tether external name;
8. external sessions are cataloged and attached only; their type has no create, persist, rename, close, kill, or prune operation.

Version 0.1 remote support is ordinary OpenSSH transport to remote `tmux`. It is not native remote Herdr federation.

## Runtime flow

```mermaid
flowchart LR
    CLI[CLI / explorer] --> Intent{create, owned resume, or external attach?}
    CLI --> Status[StatusService]
    CLI --> Discovery[DiscoveryService]
    Discovery --> Roots[bounded local / SSH root scans]
    Status --> Tmux
    Intent -->|create| Select[host + directory + command + placement]
    Select --> Durable[DurableBackend]
    Durable --> Tmux[TmuxBackend]
    Tmux -->|local argv| Local[local tmux]
    Tmux -->|BatchMode SSH + quoted remote argv| Remote[remote tmux]
    Select --> Store[ConfigStore / StateStore]
    Intent -->|resume exact ID| Context
    Intent -->|attach exact external name| Context
    Select --> Context{Herdr context?}
    Context -->|no| Attach[run attach command in current terminal]
    Context -->|yes| Herdr[HerdrClient]
    Herdr --> Pane[overlay / split / tab]
    Pane --> Resume[run session resume in returned pane]
    Resume --> Durable
```

For `open`, the CLI resolves hosts before creating or loading state when an explicit host was supplied. A fully specified create request bypasses the explorer. Otherwise, the explorer returns one of three typed intents: create obtains directory, command, and placement before creating and atomically recording a workload; owned resume carries an active record's exact `SessionId`; external attach carries the current host target and a validated non-Tether name. In Herdr context, create/owned paths place `session resume <ID>` while external paths place the hidden, attach-only `session attach-external --target … -- <name>` command. Outside Herdr, attachment runs in the current terminal.

## Actual boundaries

### `DurableBackend`

`src/backend.rs` defines the transport-independent lifecycle contract:

```rust
trait DurableBackend {
    fn create(&self, launch: &LaunchSpec) -> Result<()>;
    fn inspect(&self, id: &SessionId) -> Result<WorkloadState>;
    fn attach_command(&self, id: &SessionId) -> Result<CommandSpec>;
    fn close(&self, id: &SessionId) -> Result<()>;
}
```

`LaunchSpec` carries the generated session ID, initial directory, and trusted shell command. `CommandSpec` preserves an executable and separated argv until a boundary requires a POSIX command line. `WorkloadState` distinguishes missing, running (with attached-client count), and indeterminate workloads.

This trait owns durable workload lifecycle only. It does not own configuration, state persistence, picker UI, pane placement, or host discovery.

### `TmuxBackend`

`src/tmux.rs` is the only v0.1 `DurableBackend` implementation. Its location is either local or one validated explicit SSH target.

Local operations execute `tmux` with separated argv. Remote operations build one POSIX-quoted remote `tmux` command and pass it to OpenSSH after `--`:

- create: detached `tmux new-session`, exact generated name, selected directory, then `/bin/sh -lc <command>`;
- inspect: exact-name-filtered `tmux list-sessions` output with the attached-client count;
- attach: `tmux attach-session` for the exact target;
- close: `tmux kill-session` for the exact target.

External attachment is an inherent, non-destructive `TmuxBackend` operation rather than part of `DurableBackend`: it accepts only `ExternalSessionName` and emits `attach-session -t =<name>`. `ExternalSessionName` accepts bounded printable ASCII only and rejects empty, leading-`$`, tmux-delimiter, and reserved `tether-*` values. Leading `$` is excluded because tmux resolves server session IDs before exact-name matching. The owned trait remains the only API with create/inspect/close.

All SSH operations use `BatchMode=yes`. Interactive attach additionally requests a TTY. Server-alive probes are configured for backend operations; no setting weakens OpenSSH host-key verification. OpenSSH configuration, authentication, proxies, and `known_hosts` remain outside Tether.

The remote target validator prevents values that could become SSH options or shell syntax. The remote command builder POSIX-quotes every `tmux` argument. Session targeting uses `=<ID>` rather than a prefix.

### `StatusService`

`src/status.rs` owns one ephemeral, non-destructive tmux catalog observation per host. It snapshots hosts and active Tether IDs without holding a state lock, then runs `tmux list-sessions` through a fixed four-worker pool. One validated snapshot derives both owned workload status and attachable external rows, avoiding duplicate probes or inconsistent ownership views. Results publish independently, so one slow host does not delay completed hosts.

Each probe has a three-second monotonic deadline. Probe stdin is null, stdout/stderr are drained without blocking and capped at 64 KiB, and Linux/macOS probes run in their own process group. Timeout, refresh cancellation, or receiver drop kills and reaps the active group. Remote probes retain `BatchMode=yes`, target validation, separated SSH argv, normal host-key checking, and POSIX-quoted remote tmux argv.

The catalog parser requires UTF-8 rows shaped as exact `name<TAB>attached-count`, rejects duplicate/structurally malformed catalogs and caps rows at 256. Safe non-`tether-*` names become external rows. Every `tether-*` name is reserved: active persisted IDs derive owned status; unmatched valid IDs and malformed reserved names are hidden and counted, never offered through the external path. A structurally valid reserved collision does not make an unrelated exact owned ID indeterminate. Unsafe or unrenderable external names are skipped and counted without poisoning valid owned/external rows.

Generation-tagged messages distinguish host reachability, owned running/missing/unknown/timeout/error state, and external available/unavailable/timeout/error catalogs. Exit 255 is remote-unreachable; tmux exit 1 is a reachable empty catalog. Structural malformation or truncation yields owned `unknown` plus catalog error, never a false empty snapshot. These observations never rewrite durable session metadata.

### `DiscoveryService`

`src/discovery.rs` owns ephemeral repository discovery beneath explicit scan roots. `PickerOptions` keeps those roots separate from recent-directory suggestions: local scans use configured `discovery.local_roots` or `HOME`, and remote scans use each host's roots or `~`. A configurable bounded worker pool scans hosts independently and publishes generation-tagged repository and completion events, so local results and fast hosts remain usable while another SSH target is slow.

Local traversal is lexical, bounded by validated configured depth/entry/result/time values, prunes at repositories, accepts `.git` directories or files, and uses `symlink_metadata` so it never follows a symlink. Remote traversal runs one fixed portable `/bin/sh` scanner through BatchMode OpenSSH with validated target argv and POSIX-quoted roots; it applies the same depth, entry, result, and no-symlink constraints. Its output is a NUL-framed protocol: malformed framing, invalid root indexes, absolute paths, and parent traversal invalidate the response; an isolated non-UTF-8 path record is skipped while valid records remain available with an error status. The shared bounded process runner supplies the wall-clock deadline, 64 KiB output cap, cancellation, process-group cleanup, and null stdin.

Discovery is overlay-local and never rewrites configuration or session state. Refresh cancels both status and discovery generations, removes old discovered rows, preserves configured/recent picker suggestions, and rejects late messages.

### `HerdrClient`

`src/herdr.rs` is a local Herdr CLI adapter, not a durable backend. It consumes Herdr's executable, pane ID, and workspace ID from plugin context. It can:

- open a manifest-declared `picker` or `setup` plugin pane as an overlay;
- split and focus the invoking pane right or down;
- create a tab and extract its root pane ID;
- run one POSIX-quoted `CommandSpec` in the newly returned pane;
- validate Herdr's JSON result type and returned pane identity.

`HerdrClient` never creates, inspects, or kills the durable workload. Conversely, `TmuxBackend` knows nothing about Herdr panes. The CLI is the orchestration layer joining these boundaries.

### Configuration, state, and discovery

`ConfigStore` owns strict version-2 TOML host/root/preset, UI placement, discovery-limit/root, and closed-record retention defaults. `StateStore` owns version-1 JSON session records. `AppPaths` resolves plugin directories first and otherwise follows XDG/HOME defaults. Both stores use the shared private atomic writer and serialize public load/save/update operations with a per-file advisory lock; legacy config/state migration read, validation, and rewrite occur inside one lock transaction through private non-reentrant helpers.

`sshcfg` discovers only literal OpenSSH `Host` aliases for picker/list use. OpenSSH itself still interprets the selected alias and the rest of its configuration. An alias is synthesized as an ephemeral host option; it is not copied into Tether config.

`State` is metadata, not a process registry. Each record retains the resolved target so removing a host configuration does not make an existing record unaddressable. Active records are not automatically reconciled when a workload vanishes.

## Picker and placement boundary

`PickerOptions` merges inputs with explicit precedence:

- local first when included;
- configured hosts and then discovered non-duplicate SSH aliases;
- active Tether workload records newest-use first, followed by validated external tmux sessions, with **Create new Tether workload** last;
- recent session directories newest-use first, configured roots, then repositories discovered progressively beneath those roots;
- built-in shell first, followed by configured presets;
- configured UI placement, split right by default.

The state machine is always host → resource. The resource list contains active owned records, current external catalog rows, and **Create new Tether workload**. Owned or external selection proceeds directly to placement and returns a distinct exact-ID resume or exact-name attach intent; Create proceeds through directory → command → placement. On an owned row only, press-only `c` enters a typed red confirmation modal and only `y` emits an exact `SessionId` close request; external/Create identities cannot represent destruction. Close executes on a worker while the terminal loop remains responsive. Once confirmed, Cancel/Back/selection are suppressed until its bounded result, so process exit cannot abandon a durable transition. Success removes exactly that owned row and refreshes both generations; failure keeps it selected with a persistence-neutral retry label and a sanitized bounded full cause chain. Partial create CLI arguments suppress both owned and external choices so they cannot be silently ignored.

The terminal loop polls input at 50 ms while draining status/catalog, repository-discovery, and lifecycle-result channels. External catalog rows replace atomically per host; selection is preserved by typed identity rather than numeric index when earlier-sorting rows arrive. Refresh keeps prior external rows visibly stale and attachable while a new bounded probe runs; an exact stale attach may safely fail if the session disappeared. Cached owned status is display-only and never authorizes close. Late generations, including old close results, are rejected. Fresh absence removes external rows without mutating persisted state, and a fresh owned `missing` result disables resume.

Placement is meaningful only when Herdr context is available. In an ordinary terminal attachment happens in that terminal. In Herdr, placement focuses the requested split/tab and starts either exact-ID owned resume or the narrow exact external attach command. External attachment reads no state and invokes no lifecycle mutation. All tmux sessions remain independent of the Herdr view.

## Lifecycle and failure semantics

### Create

1. Resolve target and command.
2. Generate a UUID-v7-derived Tether session ID.
3. `DurableBackend::create` starts detached `tmux`.
4. Append active metadata and save it under the state-file advisory lock.
5. If save fails, best-effort `close` the just-created backend session. If rollback also fails, report both failures and the exact ID of the workload that may remain.
6. Attach directly or place a Herdr resume command.

A failure in step 6 leaves the workload and active metadata intact by design.

### Resume

Resume rejects unknown, closing, or closed records. Under the state-file lock it reconstructs a backend from the record's retained target and inspects the exact ID. Only `Running` is attachable; `Missing` and `Unknown` are explicit errors. The successful pre-attach path updates `last_used_at` and atomically saves it, releases the lock, then launches attach. A subsequent attach failure does not close or rewrite the record.

### Close

`LifecycleService` is shared by CLI and explorer close. It rejects unknown/already-closed IDs, reads the exact persisted target, and runs exact inspection with a three-second deadline and 64 KiB output caps without holding the state advisory lock; `Unknown` leaves an initially active record byte-identical. A known Missing/Running result is revalidated by exact ID, target, and status under a short lock and persisted as `Closing`. Exact close transport uses the same bounded process-group-cleaned runner while state remains unlocked. Missing finalizes directly; Running invokes exact-session kill first. A final short lock revalidates the same Closing record and writes `Closed` with one `last_used_at`/`closed_at` timestamp; matching peer finalization is idempotent. Kill timeout/failure, final-save failure, and conflicting changes never masquerade as success; durable Closing can retry.

### Prune

Prune computes eligibility from record status, `closed_at`, current time, and retention (30 days by default). It intentionally supplies `Missing` for an already-closed record because explicit close either proved the workload absent or killed it successfully. It does not reconnect or inspect. Dry-run and real prune print the same eligible IDs; only real prune rewrites state. Active, unknown, and recent closed records are retained.

## Persistence and security

The storage layer creates parent directories and persistent sibling advisory-lock files, enforcing Unix mode `0700` on directories and `0600` on lock/data/temporary files. Mutating CLI operations serialize load → mutation → save under the per-file lock. The atomic writer writes and syncs a unique sibling temporary file, atomically renames it over the destination, then syncs the parent directory. Config/state validation occurs before save. Missing files yield empty current-version values; supported legacy version 0 data migrates on load.

Stored metadata includes targets and trusted preset names/commands in config plus session location/lifecycle data in state. It does not include SSH passwords, private keys, terminal contents, or telemetry identifiers. There is no telemetry subsystem.
External session names and attached-client counts are overlay-local observations and are never stored. A name matching the reserved `tether-*` namespace cannot be coerced into the attach-only external type.

Security is deliberately delegated at clear boundaries:

- OpenSSH owns authentication and strict host-key policy;
- Tether validates targets, preserves argv, and quotes at the remote shell/Herdr command-line boundaries;
- `tmux` owns workload durability;
- Herdr owns local plugin context and pane creation;
- `/bin/sh -lc` executes user-configured commands, which are trusted code rather than sandboxed data.

## Capability evidence and manual boundaries

Automated tests exercise parsing/state, locked migrations/concurrency, host/repository/external discovery, bounded process cleanup, exact local/SSH argv, lifecycle recovery, Herdr placement, explorer create/resume/external transitions, status generations, and confirmed close. Close coverage includes typed owned-only confirmation, red narrow-terminal rendering, pending-exit suppression, sanitized chained errors, exact lock-free bounded transport, unknown byte identity, recoverable timeout/failure, retries, idempotent peer finalization, and external exclusion. The current run reported 97 passing tests and a locked release build.

Live verification covered Herdr 0.7.3 development link, action list, and unlink. A strict-BatchMode SSH run from Hermes to `dev` exercised remote create, real-TTY attach, detach, same-PID counter continuity, resume, exact close, and prune isolation with an unrelated `tmux` session retained.

The same run verified a directory containing spaces and literal shell metacharacters without creating an injected sentinel. A separate real-TTY run on Hermes verified the equivalent local create/attach/detach/resume/close lifecycle and unrelated-session isolation. Native Herdr action/overlay interaction, all three placements with mixed local/remote panes, picker Esc state immutability, setup's non-modification of Herdr config, and macOS live lifecycle remain acceptance checks. CI runs the complete Rust gates on Ubuntu 24.04 and macOS 14. Reproducible steps are maintained in the README's **Independent Hermes verification** section.

## Future native federation path

A future `RemoteHerdrBackend` should implement `DurableBackend`; it should not grow transport conditionals inside `HerdrClient` or replace state persistence with pane state. That backend would translate the same create/inspect/attach-command/close lifecycle into authenticated remote Herdr operations. Native federation additionally requires contracts that v0.1 does not have:

- remote Herdr identity and trust establishment;
- capability/version negotiation;
- stable remote workload and workspace identifiers;
- attachment semantics that can still produce a local `CommandSpec` or a deliberately revised backend interface;
- precise timeout, partition, retry, and idempotency behavior;
- reconciliation rules for local metadata versus remote truth.

The likely selection/orchestration shape is:

```mermaid
flowchart TD
    Selection[resolved host/backend selection] --> Factory[backend factory]
    Factory --> LocalTmux[TmuxBackend: local]
    Factory --> SshTmux[TmuxBackend: SSH tmux]
    Factory --> Native[RemoteHerdrBackend: future]
    LocalTmux --> Contract[DurableBackend lifecycle]
    SshTmux --> Contract
    Native --> Contract
    Contract --> State[existing lifecycle + StateStore]
    State --> Placement[local HerdrClient placement]
```

No `RemoteHerdrBackend` exists today. Naming it here is an architectural path, not a compatibility or delivery promise.

## Roadmap

1. Complete native Herdr placement, cancellation/config-integrity, and macOS live acceptance checks.
2. Define native federation identity, capabilities, lifecycle, and failure semantics before changing the backend trait.
3. Implement and test `RemoteHerdrBackend` only after those contracts are stable.
4. Add explicit operator-driven reconciliation for missing active workloads without weakening conservative pruning.
5. Expand release/distribution and Linux/macOS live matrices.

## Design influence

`herdr-mirror` v0.1.6, licensed MIT, was inspected as an influence on the problem space. Tether does not copy its topology or code; the `DurableBackend`/`TmuxBackend`/`HerdrClient` separation described above is this project's implementation.
