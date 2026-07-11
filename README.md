# Tether for Herdr

Tether keeps a terminal workload alive when its visible Herdr pane or SSH connection goes away. Version 0.1 creates a named `tmux` session either locally or on an SSH host, records enough metadata to find it again, and lets Herdr open the attach command in a split or tab.

> **Scope of v0.1:** remote sessions are `tmux` over ordinary OpenSSH. Tether does **not** federate Herdr instances, synchronize remote workspaces, or use a native remote Herdr protocol. See [the architecture](docs/architecture.md).

## Status and limitations

Implemented and covered by the current command/test surface:

- durable local and SSH-backed `tmux` create, inspect, attach, and explicit close;
- configured hosts plus literal aliases read from `~/.ssh/config`;
- a unified host → Tether-owned/external/create explorer with exact resume of owned workloads and non-owning attachment to safely discovered existing tmux sessions;
- progressive local/remote reachability, owned-workload status, and external tmux catalog discovery with independent three-second probe deadlines and explicit `r` refresh;
- bounded local and BatchMode-SSH repository discovery from configured per-host scan roots plus the local `HOME`/remote `~` fallback, with configurable depth/entry/result/deadline/worker bounds, progressive results, deterministic local ordering, and no symlink traversal;
- case-insensitive directory filtering and literal direct-path entry from the explorer;
- Herdr managed terminal overlays and focused split-right, split-down, or new-tab placement;
- versioned, private, advisory-locked, atomic configuration and session-state persistence, including lock-safe v0.1 migration;
- list, resume, close, age-based closed-metadata pruning, and JSON output;
- Linux and macOS declared by the plugin manifest.

Important limitations:

- Closing a Herdr pane, losing SSH, or a failed attach does not close the `tmux` workload. Use `session close` explicitly.
- Existing non-Tether tmux sessions are discovered and may be attached, but Tether never adopts, persists, renames, closes, kills, or prunes them. All `tether-*` names remain reserved and non-actionable unless they match an owned active record.
- Explorer observations are point-in-time and cached only while that overlay remains open. Refresh marks prior owned/external results visibly stale until each host completes; timeout, offline, invalid-response, error, unknown, running, and missing are distinct.
- `session prune` removes only sufficiently old records already marked `closed`; it does not kill workloads or probe/reconnect to hosts.
- Commands and presets intentionally run through `/bin/sh -lc` on the selected machine. Configure only commands you trust.
- Remote Herdr is not required. `host check` reports a remote `herdr` binary when present, but v0.1 does not use it for session transport.
- Herdr 0.7.3 does not expose native non-terminal plugin UI or sidebar extensions. Tether therefore uses the supported manifest-declared terminal overlay.

### Verification status for this run

The current branch passes 79 automated tests and a locked release build. Coverage includes lock-safe v0/v1 config and v0 state migration, concurrent config/state updates, configurable discovery/retention precedence, bounded actionable doctor probes, bounded repository discovery and adversarial remote responses, bounded external tmux catalogs, reserved-name collision handling, exact non-owning external attachment, explorer search/direct-path behavior, progressive multi-host status, timeout/cancellation/process cleanup, refresh generations/stale labels, exact owned resume IDs, and the v0.1 lifecycle suite. Tmux-hosted interactive smokes selected a newly discovered repository for creation, resumed an owned workload, and attached an external session without creating, killing, or persisting it.

For the v0.1.0 release baseline, a locked release build and Herdr 0.7.3 development link/action-list/unlink smoke passed. Independent live verification from the Hermes host to `dev` used strict BatchMode OpenSSH and exercised remote create, real-TTY attach, detach, resume, and explicit close against `tmux` 3.4. That baseline retained the same workload PID while its counter advanced after detach, covered a directory containing spaces and literal shell metacharacters without injection, and left an unrelated tmux session intact during exact close/prune checks. Native Herdr placement interaction and the macOS live lifecycle remain acceptance checks.

## Prerequisites

- Linux or macOS.
- [Herdr](https://github.com/ogulcancelik/herdr) 0.7.3 or newer for plugin actions and pane placement.
- Rust 1.88 or newer and Cargo to build from source.
- `tmux` on the local machine, and on every remote target used by Tether.
- OpenSSH `ssh`. Remote access must already work non-interactively with `BatchMode=yes`, using your normal OpenSSH keys/agent and `known_hosts` policy.

Tether does not collect passwords or private keys. Establish and verify SSH access yourself before adding a target.

## Install

Install through Herdr from the repository:

```sh
herdr plugin install moneycaringcoder/herdr-tether --yes
herdr plugin action list --plugin moneycaringcoder.tether
```

For a source checkout without installing it as a plugin:

```sh
git clone https://github.com/moneycaringcoder/herdr-tether.git
cd herdr-tether
cargo build --release --locked
./target/release/herdr-tether setup --yes
```

### Development link

From the repository root:

```sh
cargo build --release --locked
herdr plugin link "$(pwd)"
herdr plugin action list --plugin moneycaringcoder.tether
herdr plugin action invoke setup --plugin moneycaringcoder.tether
```

Unlink the development checkout with:

```sh
herdr plugin unlink moneycaringcoder.tether
```

`setup` creates Tether's files but deliberately does not modify Herdr configuration. A suggested Herdr keybinding action is:

```text
plugin_action moneycaringcoder.tether.open
```

Add that action using the keybinding syntax appropriate to your Herdr configuration; Tether will not edit it for you.

## Quick start

Initialize private config/state files and check local dependencies:

```sh
herdr-tether setup --yes
herdr-tether doctor
```

Open the interactive explorer:

```sh
herdr-tether open
```

Choose a host, then select an active Tether workload, a safely discovered **external** tmux session, or **Create new Tether workload**. Owned and external rows are always labeled; external attached-client counts are observations, not ownership. Repository scans append creation directories progressively. Host rows progress from `loading` to `online`, `offline`, `timeout`, or `error`; owned workload rows distinguish `running`, `missing`, `unknown`, `timeout`, and `error`. Press `r` to refresh status, external catalogs, and repositories. Existing external rows remain visibly stale and attachable during refresh; an exact attach may safely fail if the session disappeared. Freshly missing owned workloads cannot be resumed. Closing and closed Tether records are not resumable, and reserved `tether-*` collisions are hidden and non-actionable.

Or create a local session without the explorer:

```sh
herdr-tether open \
  --host local \
  --directory "$HOME" \
  --command 'exec ${SHELL:-/bin/sh}'
```

Add and check a remote target, then open it:

```sh
herdr-tether host add dev dev@example.com \
  --root /srv/project \
  --preset 'shell=exec ${SHELL:-/bin/sh}'
herdr-tether host check dev
herdr-tether open --host dev --directory /srv/project --preset shell
```

Use a literal `Host` alias from `~/.ssh/config` without adding it first:

```sh
herdr-tether host list
herdr-tether open --host my-ssh-alias --directory '~' --command 'exec ${SHELL:-/bin/sh}'
```

Tether prints the generated session ID. Detach normally from `tmux`; later:

```sh
herdr-tether session list
herdr-tether session resume tether-…
herdr-tether session close tether-…
```

## Command reference

```text
herdr-tether setup [--yes]
```

Creates or validates the versioned config and state, then prints their paths, effective discovery/retention/placement defaults, configured-target count, required prerequisites, binding guidance, and the next doctor command. `--yes` is required when standard input is not a terminal. Setup is idempotent, does not probe remote targets, and never edits Herdr or SSH configuration.

```text
herdr-tether host add <NAME> <TARGET> [--root <DIR>]... [--preset <NAME=COMMAND>]...
herdr-tether host list [--json]
herdr-tether host remove <NAME>
herdr-tether host check <NAME>
```

- `add` saves an explicit OpenSSH target, optional picker roots, and optional command presets. Targets may be a literal host/alias, `user@host`, or an `ssh://` authority with optional user and port; SSH options, whitespace, paths, queries, fragments, and shell punctuation are rejected. Names and preset names must be unique; `local` is reserved.
- `list` emits `local`, configured hosts, and non-duplicated literal aliases discovered from `~/.ssh/config`. Text fields are tab-separated; `--json` emits objects with `name`, `target`, and `source`.
  Discovery reads literal `Host` entries from the primary file only; it does not traverse `Include` directives. Add an included alias explicitly with `host add` if it is not listed.
- `remove` removes only the host configuration. Existing session records retain their resolved target and remain addressable.
- `check` runs local `tmux -V`, or remote `tmux -V` through BatchMode OpenSSH. A remote Herdr version probe is optional.

```text
herdr-tether open [--host <NAME>] [--directory <DIR>]
                  [--command <COMMAND> | --preset <NAME>]
                  [--placement split-right|split-down|new-tab]
```

Supplying host, directory, and exactly one command source bypasses the explorer. Otherwise the explorer returns a typed create or resume intent and applies any supplied arguments as overrides. On the directory stage, `/` opens a case-insensitive filter and `p` opens literal direct-path entry; `Enter` applies the input and `Esc` closes it without cancelling the explorer. Outside Herdr, Tether runs the attach command in the current terminal. In Herdr plugin context, it creates the chosen split/tab and runs `session resume <ID>` there.

```text
herdr-tether session list [--json]
herdr-tether session resume <ID>
herdr-tether session close <ID>
herdr-tether session prune [--dry-run] [--older-than-days <DAYS>]
```

- `list` shows persisted metadata; JSON includes the complete records.
- `resume` refuses closed, missing, or indeterminate workloads; a valid attempt updates `last_used_at` before attaching.
- `close` is the only lifecycle command allowed to invoke `tmux kill-session`. It inspects first: a workload proven missing is marked closed without a kill; a running workload is first marked `closing`, then killed by exact session ID and finalized as closed. A failed kill or final state write leaves the recoverable `closing` marker for a later `close` retry; an indeterminate workload is left unchanged.
- `prune` prints eligible IDs and removes only closed metadata at least `retention.closed_days` old (30 days in a migrated/default config). `--older-than-days` overrides the configured value; `--dry-run` only prints. An explicit `--older-than-days 0` makes every already-closed record eligible.

```text
herdr-tether doctor
```

Reports config/state presence and usability, then probes required `tmux`, `ssh`, Cargo, and the selected Herdr executable with three-second deadlines and 64 KiB output caps. Missing, permission-denied, nonzero, timeout, unusable-file, and incomplete Herdr plugin-context failures are actionable and make doctor exit nonzero without suppressing later checks. An unrelated standalone `PANE_ID` does not imply plugin context. This is a local health report; use `host check <NAME>` for an end-to-end SSH target test.

The hidden `plugin open` and `plugin setup` commands are manifest action entrypoints. They require Herdr-provided pane/workspace/binary environment and open the corresponding managed overlay; they are not normal standalone commands.

## Picker and Herdr placement

The explorer proceeds through:

1. host (`local`, configured hosts, then discovered SSH aliases);
2. active Tether workloads, safely discovered external tmux sessions, then **Create new Tether workload**;
3. directory for creation (most recently used first, configured roots, then repositories discovered asynchronously beneath those roots);
4. built-in shell or a host preset for creation;
5. split right, split down, or new tab.

Keys: `↑`/`k`/`Shift-Tab` previous, `↓`/`j`/`Tab` next, `Enter`/`→` confirm, `Backspace`/`←` back, `r` refresh, and `Esc`/`Ctrl-C` cancel. On the directory stage, `/` filters and `p` enters a literal path; while editing, printable keys insert text, `Backspace` deletes, `Enter` applies, and `Esc` returns to the list. The default placement is split right.

Plugin actions first open `picker` or `setup` as a managed overlay. After selection, Tether asks Herdr to create exactly one split or tab and runs either exact-ID owned resume or exact-name external attach in the returned pane. The tmux session remains independent of that Herdr view. External attachment invokes only `attach-session -t =<name>`; it cannot enter Tether create/close/prune or persistence paths.

## Persistence and cleanup

Default paths outside plugin context:

- config: `${XDG_CONFIG_HOME:-$HOME/.config}/herdr-tether/config.toml`
- state: `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-tether/state.json`

Inside a plugin, `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR` independently take precedence. Missing files load as version-2 config and version-1 state defaults. Existing v0/v1 config and v0 state migrate and rewrite while holding the same per-file advisory lock used by save/update transactions. On Unix, parent directories are forced to mode `0700`; data, temporary, and advisory-lock files are mode `0600`. Writes use a same-directory temporary file, file sync, atomic rename, and directory sync.

The generated TOML config is the settings source; JSON state never needs hand editing. Host roots/presets can also be managed with `host add`.

```toml
version = 2
hosts = []

[ui]
placement = "split-right"

[discovery]
local_roots = []
max_depth = 4
max_entries = 4096
max_results = 64
timeout_seconds = 3
workers = 4

[retention]
closed_days = 30
```

An empty `local_roots` uses `HOME`; `~` and `~/…` entries expand against `HOME`. Remote hosts scan their configured `roots`, or `~` when none are configured. Recent session directories remain picker suggestions but do not silently expand the configured scan scope.
Configured `timeout_seconds` must be between 1 and 3600.

State stores IDs, resolved target, host label, directory, preset name, lifecycle status, and timestamps—not terminal output or credentials. If state persistence fails after creating a workload, Tether attempts to close that newly created workload. Failed attach/resume does not implicitly kill it.
External tmux sessions are never written to state. Their validated names and attached-client counts exist only in the current explorer invocation.

Cleanup is intentionally conservative. Only `session close` marks a record closed, either after inspection proves the workload missing or after exact-session `kill-session` succeeds. `session prune` assumes such a closed workload is already absent and removes old metadata without reconnecting. Active, unknown, and recently closed records are retained.

## Security model

- SSH authentication, agent use, proxying, and host-key verification are OpenSSH's responsibility. Tether passes `BatchMode=yes`; it does not weaken `StrictHostKeyChecking`, auto-accept host keys, or prompt/store credentials.
- Remote interactive attaches request a TTY and use `ServerAliveInterval=15` plus `ServerAliveCountMax=3`. Losing that connection does not kill `tmux`.
- Explicit targets are validated to reject option-like, whitespace/control, and shell-injection forms. `tmux` arguments and Herdr pane commands are kept as argv and POSIX-quoted at process boundaries.
- Session IDs are exact `tmux` targets, so a close cannot prefix-match a different session.
- Configured commands are trusted code and run under `/bin/sh -lc`. Tether provides transport quoting, not a command sandbox.
- Tether contains no telemetry and has no secret store. Its network activity is the OpenSSH traffic requested by host checks and remote lifecycle operations; Herdr integration invokes the local Herdr executable.

## Troubleshooting

### `Host key verification failed`

Tether preserves strict OpenSSH behavior. Verify the host and add its key through your organization's trusted process, then prove BatchMode access before retrying:

```sh
ssh -o BatchMode=yes -- dev@example.com 'tmux -V'
herdr-tether host check dev
```

Do not work around this with a global `StrictHostKeyChecking=no`.

### `Permission denied` or an authentication prompt is needed

Tether uses `BatchMode=yes`. Load the correct key into your agent or configure a non-interactive `IdentityFile`, `ProxyJump`, and related options in `~/.ssh/config`; then rerun the direct probe above.

### `setup requires --yes when standard input is not a terminal`

Use `herdr-tether setup --yes` from scripts and plugin automation.

### `session … no longer exists` / `could not determine whether … exists`

The metadata is active but the backing `tmux` session is missing or could not be inspected. Tether will not silently rewrite or prune that active record. Diagnose SSH/tmux access first. `prune` is intentionally not an active-record repair command.

### Picker cancellation reports an error

`Esc` and `Ctrl-C` cancel without creating a session. The CLI reports `session selection was cancelled`; this is expected and should leave state unchanged.

### Plugin context is not available

Run plugin actions through Herdr, not `herdr-tether plugin …` directly. Check:

```sh
herdr plugin action list --plugin moneycaringcoder.tether
herdr plugin action invoke open --plugin moneycaringcoder.tether
```

### A pane closed but the workload is still running

That is the durability contract. Reopen with `session resume <ID>` or intentionally terminate it with `session close <ID>`.

## Independent Hermes verification

The following is the reproducible verification checklist. Items 1–3 and 7 were completed under real TTYs with strict host-key checking where applicable; items 4–6 and 8 remain release/user acceptance checks. Use disposable SSH targets and directories when repeating it:

```sh
export TARGET=dev@example.com
ssh -- "$TARGET" 'tmux -V'
ssh -o BatchMode=yes -- "$TARGET" 'tmux -V'
herdr-tether host add hermes-live "$TARGET" --root '/tmp/tether dir with spaces'
herdr-tether host check hermes-live
```

Then record evidence for every item:

1. **Adversarial remote quoting, with no injected file.** Prepare the exact directory and sentinel, then open a command containing shell metacharacters only as literal data:
   ```sh
   ssh -o BatchMode=yes -- "$TARGET" \
     "rm -f /tmp/tether-injected; mkdir -p -- '/tmp/tether dir with spaces'"
   herdr-tether open --host hermes-live \
     --directory '/tmp/tether dir with spaces' \
     --command 'printf "%s\n" "literal ; touch /tmp/tether-injected"; exec ${SHELL:-/bin/sh}'
   ssh -o BatchMode=yes -- "$TARGET" \
     'test ! -e /tmp/tether-injected && printf "no injected file\n"'
   ```
   Confirm the literal text is printed, `pwd` is the exact directory, and the sentinel remains absent.
2. **Remote PID/counter continuity after pane close or network loss.** Start this workload and retain the printed Tether ID:
   ```sh
   herdr-tether open --host hermes-live \
     --directory '/tmp/tether dir with spaces' \
     --command 'printf "%s\n" "$$" > /tmp/tether-hermes.pid; n=0; while :; do n=$((n+1)); printf "%s\n" "$n" > /tmp/tether-hermes.counter; sleep 1; done'
   ssh -o BatchMode=yes -- "$TARGET" \
     'printf "pid="; cat /tmp/tether-hermes.pid; printf "counter="; cat /tmp/tether-hermes.counter'
   ```
   Close only the Herdr pane or interrupt the SSH client, wait at least five seconds, and rerun the SSH read. The PID must be identical and the counter larger. `herdr-tether session resume <ID>` must attach to that same workload.
3. **Close/prune isolation.** On the remote target, create an unrelated exact-name control session:
   ```sh
   ssh -o BatchMode=yes -- "$TARGET" \
     'tmux new-session -d -s tether-unrelated -- sleep 600'
   herdr-tether session close <ID>
   ssh -o BatchMode=yes -- "$TARGET" \
     'tmux has-session -t =tether-unrelated'
   herdr-tether session prune --dry-run --older-than-days 0
   herdr-tether session prune --older-than-days 0
   ssh -o BatchMode=yes -- "$TARGET" \
     'tmux has-session -t =tether-unrelated'
   ```
   Closing must remove only the exact Tether session; dry-run must not change metadata; real prune must remove only the closed Tether record; the unrelated session must survive every step.
4. **Herdr overlay, all placements, and mixed locality.** Run `herdr plugin action invoke open --plugin moneycaringcoder.tether` for three separate selections and choose split right, split down, and new tab. Leave at least one local and one remote Tether pane open together. Verify each resume command runs in the pane Herdr just returned, then close those panes and confirm their `tmux` workloads remain resumable.
5. **Cancel is side-effect free.** Locate the state path printed by `herdr-tether setup --yes`. Save both `sha256sum <state.json>` (or `shasum -a 256` on macOS) and `herdr-tether session list --json`. Invoke the picker and press `Esc` at host, directory, command, and placement in four separate runs. Both saved values must remain identical after every cancellation.
6. **Setup does not edit Herdr config.** Save a checksum of the actual Herdr config, run `herdr-tether setup --yes`, then run `herdr plugin action invoke setup --plugin moneycaringcoder.tether`. Recompute after each operation; the Herdr config checksum must remain identical.
7. **Local real-TTY attach (completed on Hermes).** From an interactive terminal, open a local workload, verify attachment succeeds, detach, resume the same ID, and explicitly close it. Keep an unrelated local `tmux` session throughout and verify exact-session isolation.
8. **macOS build/action smoke.** On supported macOS run `cargo build --release --locked`, `herdr plugin link "$(pwd)"`, `herdr plugin action list --plugin moneycaringcoder.tether`, both action invocations, local create/detach/resume/close, and split-right/split-down/new-tab placement. Unlink after evidence collection.

After verification, remove the disposable host entry and sessions. Do not report native Herdr placement or macOS live behavior until the corresponding checks above succeed.

Post-verification publishing actions, not performed on this branch:

- add the GitHub repository topic `herdr-plugin`;
- verify `herdr plugin install moneycaringcoder/herdr-tether` and the installed Open/Setup action listing against the published repository.

## Roadmap

Future work, not v0.1 behavior:

- a `RemoteHerdrBackend` that implements the durable backend contract through native Herdr federation rather than SSH-launched remote `tmux`;
- remote workspace discovery, identity, capability negotiation, and failure semantics suitable for that backend;
- explicit reconciliation tools for active metadata whose workloads are missing;
- release/distribution hardening and broader Linux/macOS live matrices.

The architecture keeps this path open without pretending it already exists.

## Influence and license

Tether is MIT licensed. `herdr-mirror` v0.1.6 (MIT) was inspected as product/design influence only; Tether copies neither its topology nor its code.
