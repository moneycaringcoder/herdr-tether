# Configuration

Tether keeps configuration separate from workload state. Prefer the CLI for host changes; edit TOML only when you need discovery, placement, preset, or retention settings not exposed by a command.

## Which files does Tether use?

Inside a Herdr plugin action, Herdr supplies authoritative plugin configuration and state directories. Standalone commands use those same directories only when `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR` are set.

Outside plugin context, defaults are:

| Data | Default path |
| --- | --- |
| Configuration | `${XDG_CONFIG_HOME:-$HOME/.config}/herdr-tether/config.toml` |
| State | `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-tether/state.json` |

Run `herdr-tether setup --yes` to create missing stores, or `herdr-tether doctor` to report which context and dependencies are active.

Tether creates data directories with Unix mode `0700` and files with mode `0600`. Writes use advisory locks, a synchronized same-directory temporary file, atomic replacement, and parent-directory synchronization. Do not edit configuration or state while Tether is running a mutating command.

## Hosts

Add an explicit SSH target with one or more repository roots and command presets:

```sh
herdr-tether host add build build.example.net \
  --root /srv/repos \
  --root '~/work' \
  --preset 'editor=exec vi' \
  --preset 'tests=cargo test'
herdr-tether host check build
```

`~` and `~/...` roots expand in the selected remote login environment. Host names are friendly picker labels; targets are OpenSSH destinations. A target cannot begin with an option or contain shell syntax.

List effective targets, including discovered literal SSH aliases:

```sh
herdr-tether host list
herdr-tether host list --json
```

Remove only the configuration entry with `herdr-tether host remove build`. Existing owned records retain their resolved target and remain visible in an owned-only group; Tether does not retarget or contact them through a different host definition.

Tether discovers literal `Host` aliases in the primary `~/.ssh/config`. OpenSSH still interprets the complete selected configuration, but Tether does not traverse `Include` directives when building its picker list. Explicitly add an included-only alias if needed.

## Configuration schema

A default version-3 configuration is:

```toml
version = 3
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

Unknown fields and invalid values are rejected rather than ignored.

### Host entries and presets

The equivalent TOML for the earlier `build` example is:

```toml
[[hosts]]
name = "build"
target = "build.example.net"
roots = ["/srv/repos", "~/work"]

[[hosts.presets]]
name = "editor"
command = "exec vi"
herdr_agent = "codex"

[[hosts.presets]]
name = "tests"
command = "cargo test"
```

Presets are trusted code. Tether starts a login `/bin/sh`, restores the selected directory, then runs the configured command through `/bin/sh -c`. It does not sandbox or escape a command into harmless data. Restrict write access to the configuration and review shared presets.

`herdr_agent` is optional. Set it to a Herdr-supported agent kind such as
`codex` when that preset runs an agent behind Tether's `tmux` or SSH boundary.
Tether then attaches with an explicit `HERDR_AGENT=<kind>` screen-manifest
hint, allowing Herdr 0.7.5 and newer to recognize the agent without changing
the durable workload lifecycle. Ad hoc creation supports the equivalent
`herdr-tether open --command COMMAND --herdr-agent KIND` option. Agent kinds
must match `[a-z][a-z0-9_-]{0,31}`; Tether never guesses one from a command.

### Placement

`ui.placement` accepts:

- `split-right` (default)
- `split-down`
- `new-tab`
- `replace-current-pane`

Replace current pane first checks for foreground processes and requests interactive confirmation when closing the source could terminate them. It creates and verifies the destination before closing the captured source. Cancellation or a pre-close failure preserves the source pane.

### Discovery

`discovery.local_roots` limits local repository scanning to explicit roots. When it is empty, Tether uses the local home directory. Remote hosts use their `roots`, or remote `~` when no roots are configured.

Discovery is bounded by depth, visited entries, results, time, and worker count. It does not follow symbolic links. Recent directories may appear as picker suggestions, but they never silently expand the configured scan roots.

### Retention

`retention.closed_days` controls when safely finalized ended history becomes eligible for metadata-only cleanup. The default is 30 days and the value must be positive. Cleanup does not contact a host, invoke `tmux`, or stop a workload.

## State is not configuration

State records Tether-owned identity, resolved host target, directory, launch information, lifecycle status, and timestamps. It does not store SSH passwords, private keys, access tokens, terminal contents, or telemetry identifiers.

Do not hand-edit `state.json`. Lifecycle transitions are lock-protected and validated so an interrupted Stop or Restart can be reconciled safely. Supported older schemas migrate atomically; a corrupt or future schema fails closed.

For the meaning of each state, see [Lifecycle](lifecycle.md). For storage and trust boundaries, see [Architecture and security](architecture.md).
