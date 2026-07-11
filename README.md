# Tether for Herdr

Tether keeps terminal workloads alive when the Herdr pane or SSH connection viewing them goes away. It gives local and SSH-backed `tmux` work a durable identity, discovers repositories and existing sessions, reports status without attaching, and resumes work in a Herdr split or tab.

Tether complements [Herdr](https://github.com/ogulcancelik/herdr) and `herdr-mirror`: Herdr owns the local terminal surface, `herdr-mirror` streams remote Herdr workspaces, and Tether manages ordinary local or SSH-backed `tmux` workloads. Remote Tether sessions use OpenSSH and `tmux`; Tether does not federate Herdr instances or stream remote Herdr panes.

## Install

### Prerequisites

- Linux or macOS.
- [Herdr](https://github.com/ogulcancelik/herdr) 0.7.3 or newer.
- Git, Rust 1.88 or newer, and Cargo. Herdr clones the repository and Cargo builds the plugin from source.
- `tmux` 3.3 or newer locally and on every remote target used by Tether.
- OpenSSH `ssh`. Remote access must already work non-interactively with `BatchMode=yes` under your normal keys, agent, and `known_hosts` policy.
- Network access to GitHub and Cargo's configured registry during installation, unless both sources and dependencies are already cached.

Install the pinned v0.2.0 release tag through Herdr:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.2.0 --yes
herdr plugin action list --plugin moneycaringcoder.tether
```

Herdr uses its managed checkout for plugin actions; it does not add the built binary to `PATH`. Install the same tagged source with Cargo when you also want the standalone `herdr-tether` commands used below:

```sh
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.2.0 --locked herdr-tether
```

Run **Tether: Install prefix+t launcher** once from Herdr, or invoke the action explicitly:

```sh
herdr plugin action invoke setup --plugin moneycaringcoder.tether
```

That explicit action creates Tether's private configuration/state, then installs `prefix+t` for `moneycaringcoder.tether.open`. It first saves the exact Herdr config to a sibling Tether backup. An identical binding is an idempotent content no-op that still requests Herdr config reload. A conflicting `prefix+t` binding, invalid/unmergeable config, or unrelated existing backup stops without overwriting the config or exposing the conflicting command. A matching backup from an interrupted attempt is safely replaced. After reload, `prefix+t` opens Tether.

The standalone equivalent is:

```sh
herdr-tether setup keybinding
```

Restore the exact backup, consume it, and reload Herdr with the command below. Rollback refuses if the config has changed since installation, preserving later user edits:

```sh
herdr-tether setup keybinding --rollback
```

Plain standalone `herdr-tether setup` and normal install/open remain non-mutating. Herdr 0.7.3 has no generic plugin-action menu, so zero-command first invocation is unavailable; the single explicit setup action above is the supported one-time bootstrap.

### Update, reinstall, uninstall, and rollback

Installing the same pinned ref again rebuilds and reinstalls the plugin. Reinstall the standalone binary with `--force` after repairing a toolchain or dependency cache:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.2.0 --yes
cargo install --force --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.2.0 --locked herdr-tether
```

To update, replace `v0.2.0` in both install commands with a newer released tag. To roll back, reinstall an older released tag, for example:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.1.0 --yes
cargo install --force --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.1.0 --locked herdr-tether
```

Configuration and state are retained independently of the plugin checkout. Back them up before moving to an older release because migrated data may not be understood by that release.

Uninstall the plugin with:

```sh
herdr plugin uninstall moneycaringcoder.tether
```

Remove the standalone binary, if installed, with `cargo uninstall herdr-tether`.

Uninstalling does not intentionally terminate `tmux` workloads. Close owned workloads first if you no longer need them. Remove the configuration and state files shown by `herdr-tether setup --yes` (and their parent `herdr-tether` directories) separately if you also want to discard metadata.

Installing from mutable `main` is development-only and may contain unreleased changes:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref main --yes
```

For local development, clone the repository, run `cargo build --release --locked`, and use `herdr plugin link "$(pwd)"`; remove that link with `herdr plugin unlink moneycaringcoder.tether`.

### v0.2.1 release candidate

The branch metadata is v0.2.1, but v0.2.0 remains the pinned stable install above until v0.2.1 is accepted and tagged. The candidate adds explicit keybinding installation and rollback, Replace current pane, more reliable executable and working-directory handling, stable in-picker retry/cancel errors, and session-scoped mouse support for Tether-owned sessions.

## Quick start

Open **Tether: Open** in Herdr. Choose a host, then an owned workload, a safely discovered **external** `tmux` session, or **Create new Tether workload**. Creation proceeds through directory, command, and split-right/split-down/new-tab/Replace current pane placement.

Herdr injects plugin-owned configuration and state directories into actions. Point the standalone CLI at those same Linux/macOS directories before configuring data that **Tether: Open** must read:

```sh
export HERDR_PLUGIN_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/herdr/plugins/config/moneycaringcoder.tether"
export HERDR_PLUGIN_STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/moneycaringcoder.tether"
herdr-tether setup --yes
herdr-tether doctor
```

Without those exports, standalone commands intentionally use their separate `herdr-tether` configuration and state directories. With the plugin directories still exported, add and verify a remote target:

```sh
herdr-tether host add build build.example.net \
  --root /srv/repos \
  --preset 'editor=exec vi'
herdr-tether host check build
```

Tether can also use literal aliases from the primary `~/.ssh/config` without copying them into its configuration. It does not traverse OpenSSH `Include` directives; add an included alias explicitly if it is not listed by `herdr-tether host list`.

## Three workflows Tether makes easier

### 1. Discover a repository, launch work, leave, and resume

After configuring the `build` host above, invoke **Tether: Open** and choose:

```text
build → Create new Tether workload → <discovered repository> → editor → Split right
```

Tether scans configured roots within bounded limits, creates a uniquely named detached `tmux` session in the selected directory, verifies the created pane's exact working directory, enables mouse support only on that owned session, records its exact host and target, creates the requested Herdr view, and attaches there. A directory mismatch fails creation and triggers exact-session cleanup instead of recording a workload in the wrong directory. Closing that view or losing SSH leaves the workload running. Reopen Tether and select its `[active]` row to resume it in any placement; resume re-enables mouse only for the owned session, while external attachment never changes session options.

Manually, this requires remembering the SSH destination and directory, naming and later finding the `tmux` session, opening the Herdr pane, and composing the exact attach command. `herdr-mirror` can mirror a remote Herdr pane, but it does not discover arbitrary `tmux` work or provide Tether's owned lifecycle and metadata cleanup.

### 2. Inspect local and remote work without attaching

Pause on the explorer's Hosts or Resources stage. Hosts complete independently as `online`, `offline`, `timeout`, or `error`; loading and stale results remain explicit. Press `r` to refresh.

For automation, use the read-only snapshot:

```sh
herdr-tether snapshot --pretty
```

Snapshot schema version 1 joins host reachability, effective discovery roots and repositories, complete owned metadata and live Active status, and safe external catalogs. Expected degradation is typed partial data rather than a fabricated empty result. It never includes preset command bodies, child output, raw backend errors, or private storage paths, and it has no lifecycle or persistence-mutation capability.

The manual equivalent is a host-by-host sequence of SSH checks, `tmux list-sessions`, repository scans, and hand-maintained joins. `herdr-mirror` reports remote Herdr workspace state; Tether also covers local and ordinary SSH-backed `tmux` without requiring a remote Herdr server.

### 3. Find and attach an existing tmux session safely

Select an `[external · running]` row, then choose split right, split down, or new tab. Tether validates the name and attaches by exact target. It never adopts, persists, renames, closes, kills, marks, or prunes an external session; closing its Herdr view only detaches that client.

Manually, this requires listing and interpreting sessions, carefully quoting the exact target, and creating and focusing the Herdr view. Tether combines those steps while preserving the ownership boundary.

| Need | Manual SSH + tmux | herdr-mirror | Tether |
| --- | --- | --- | --- |
| Remote prerequisite | SSH and tmux | SSH and a remote Herdr server | SSH and tmux |
| Find repositories | Hand-written commands | Not its purpose | Bounded configured-root scans, filtering, and direct paths |
| Inspect without attaching | Per-host commands | Remote Herdr workspace status | Local/remote reachability, owned status, and external catalogs |
| Durable identity | User-managed names | Herdr pane/workspace identity | Generated ID and retained history for owned work |
| Existing tmux sessions | Manual list and attach | Only through remote Herdr | Validated exact attach without ownership |
| Close behavior | Manual | Configurable mirror/remote coupling | Confirmed exact close for owned work only |
| Presentation | User creates panes | Continuously mirrored workspace | One overlay; focused right/down split or new tab |

## Explorer and lifecycle

The explorer stages are host → resource; creation continues through directory → command → placement. Current targets show Active, Closing, and Closed owned records, safe external sessions, and the create row. Records retained from a removed or retargeted host appear in explicit owned-only groups and are never contacted by background status or discovery.

- **Active:** resumable and closeable.
- **Closing:** non-resumable; exact close may be retried.
- **Closed:** metadata only; eligible for pruning after the retention period.
- **External:** attachable but never owned or destructively managed by Tether.

On an Active or Closing row, press `c`, then `y` to confirm exact close. Press uppercase `P`, then `y` to prune eligible Closed metadata globally. Prune has no SSH or `tmux` capability and removes only unchanged records from the confirmed preview. Closing cannot be started from external rows, and prune never affects external sessions or running workloads.

Explorer keys: `↑`/`k`/`Shift-Tab`, `↓`/`j`/`Tab`, `Enter` to select, `Backspace` to go back, `r` refresh, and `Esc`/`Ctrl-C` cancel. Left and Right never select, advance, or launch. On the directory stage, `/` filters and lowercase `p` accepts a literal path. A failed create, resume, attach, or placement remains as a stable modal: only Enter retries the exact selection, while Backspace or Esc dismisses the error and returns to Hosts.

Herdr actions initially open a manifest-declared terminal overlay. After selection, Tether focuses the invoking pane's requested split or tab and starts the exact resume/attach command there. **Replace current pane** first inspects foreground processes and requires interactive confirmation before terminating any it finds. It then creates a destination, dispatches the attachment, waits for foreground-process readiness, and only then closes the captured source pane. Dispatch or readiness failure cleans up the destination and preserves the source; a source-close failure preserves the running destination. Non-interactive replacement refuses when foreground processes require confirmation, and cancelling preserves the source.

Herdr 0.7.3 does expose public semantic reporting commands (`pane report-agent`, `report-agent-session`, `release-agent`, and `report-metadata`), but those APIs are assertions about a known agent lifecycle rather than generic nested-workload registration. Tether knows its outer `tmux` identity and attach lifetime; it cannot truthfully infer a nested OMP or Hermes lifecycle or native session identity, and tmux detach is not agent completion. Tether therefore uses honestly titled terminal panes as its fallback and does not fabricate native sidebar or Agents support. A native inner hook must make any OMP or Hermes report itself.

## Command reference

```text
herdr-tether setup [--yes]
herdr-tether setup keybinding [--rollback]
herdr-tether doctor
herdr-tether host add <NAME> <TARGET> [--root <DIR>]... [--preset <NAME=COMMAND>]...
herdr-tether host list [--json]
herdr-tether host remove <NAME>
herdr-tether host check <NAME>
herdr-tether open [--host <NAME>] [--directory <DIR>]
                  [--command <COMMAND> | --preset <NAME>]
                  [--placement split-right|split-down|new-tab|replace-current-pane]
herdr-tether session list [--json]
herdr-tether session resume <ID>
herdr-tether session close <ID>
herdr-tether session prune [--dry-run] [--older-than-days <DAYS>]
herdr-tether snapshot [--pretty]
```

A fully specified `open` request bypasses the explorer. Outside Herdr, its attachment runs in the current terminal. Commands and presets are trusted code: Tether starts `/bin/sh -lc` to load the selected machine's login environment, then explicitly restores the selected directory before executing the command through `/bin/sh -c`.

When launched from a GUI with a reduced `PATH`, Tether resolves bare `tmux` and `ssh` names through absolute `PATH` entries, then `/usr/bin`, `/bin`, `/opt/homebrew/bin`, and `/usr/local/bin`. Commands placed into Herdr use the resolved current Tether executable rather than assuming `herdr-tether` is on the pane's `PATH`.

`session close` inspects and closes only the exact owned ID. An indeterminate inspection leaves Active metadata unchanged. A recoverable `closing` marker prevents a failed or timed-out kill/save from masquerading as Active; rerun close to reconcile it. Closing a Herdr pane or a failed attachment never implicitly kills the workload.

`session prune` removes sufficiently old Closed metadata (30 days by default), not workloads. `--older-than-days` overrides the configured retention and `--dry-run` changes nothing.

## Configuration and persistence

Outside plugin context, default paths are:

- config: `${XDG_CONFIG_HOME:-$HOME/.config}/herdr-tether/config.toml`
- state: `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-tether/state.json`

Herdr's authoritative plugin config and state directories take precedence in plugin context. Config schema version 2 stores hosts, presets, UI placement, bounded discovery settings, and closed-record retention. State schema version 1 stores owned session IDs, resolved targets, directories, preset labels, lifecycle status, and timestamps—not terminal output or credentials.

Existing version-0/version-1 config and version-0 state are migrated under the same advisory lock used for normal writes. Writes use private Unix permissions, a same-directory temporary file, synchronization, and atomic rename. Recent directories remain picker suggestions but never silently widen configured discovery roots.

A minimal generated config is:

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

## Security and troubleshooting

OpenSSH owns authentication, proxies, and host-key verification. Tether passes `BatchMode=yes`, never weakens strict host-key checking, and does not collect passwords, private keys, tokens, or telemetry. Explicit targets and external session names are validated, exact tmux targets are used, and argv is preserved or POSIX-quoted at process boundaries. See the [security policy](SECURITY.md) and [architecture](docs/architecture.md) for the complete trust boundary.

If a host key or authentication check fails, establish trusted non-interactive access yourself before retrying:

```sh
ssh -o BatchMode=yes -- build.example.net 'tmux -V'
herdr-tether host check build
```

Do not work around failures with global `StrictHostKeyChecking=no`. If Active metadata refers to a missing or indeterminate workload, diagnose SSH/`tmux` access first; prune is intentionally not a repair command for Active records.

## License and influence

Tether is [MIT licensed](LICENSE). `herdr-mirror` v0.1.6 (MIT) was inspected as product and design influence only; Tether copies neither its topology nor its code.
