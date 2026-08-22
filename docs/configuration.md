# Configuration

Tether keeps configuration separate from workload state. Prefer the CLI for host changes; edit TOML only when you need discovery, placement, preset, or retention settings not exposed by a command.

## Which files does Tether use?

Inside a Herdr plugin action, Herdr supplies authoritative plugin configuration and state directories. Standalone commands use those same directories only when `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR` are set.

Outside plugin context, defaults are:

| Data | Default path |
| --- | --- |
| Configuration | `${XDG_CONFIG_HOME:-$HOME/.config}/herdr-tether/config.toml` |
| State | `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-tether/state.json` |
| Agent sidebar preference | `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-tether/agent-view.json` |

Run `herdr-tether setup --yes` to create missing stores, or `herdr-tether doctor` to report which context and dependencies are active.

Tether creates data directories with Unix mode `0700` and files with mode `0600`, including directories it creates under a symlinked or GNU Stow ancestor. A directory that already exists keeps its own permissions when it is reached through a link, so Tether never re-modes a dotfiles layout you manage. Writes use advisory locks, a synchronized same-directory temporary file, atomic replacement, and parent-directory synchronization. Do not edit configuration or state while Tether is running a mutating command.

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

Tether discovers literal `Host` aliases in the primary `~/.ssh/config` and
bounded, cycle-safe `Include` files whose canonical paths remain beneath
`~/.ssh`. Wildcard `Host` patterns and escaping includes are ignored. OpenSSH
still interprets the complete selected configuration when connecting.

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

[notifications]
agent_blocked = true
agent_done = true
workload_failed = true
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
hint, allowing Herdr to recognize the agent without changing the durable
workload lifecycle. Ad hoc creation supports the equivalent
`herdr-tether open --command COMMAND --herdr-agent KIND` option. Agent kinds
must match `[a-z][a-z0-9_-]{0,31}`; Tether never guesses one from a command.

The kind is a free-form token that Herdr resolves, so agents Herdr adds later
work without a Tether release. Herdr 0.8.0 recognizes `agy`, `amp`, `claude`,
`cline`, `codex`, `copilot`, `cursor`, `devin`, `droid`, `gemini`, `grok`,
`hermes`, `kilo`, `kimi`, `kiro`, `maki`, `opencode`, `pi`, and `qodercli`.
Herdr owns the authoritative list and updates it independently of Tether; run
`herdr server agent-manifests` to see what your installed Herdr recognizes.

A kind Herdr does not recognize never changes the durable workload lifecycle,
but it also produces no sidebar agent row and no Mission Control binding. Tether
warns when it can reach Herdr and the kind is unrecognized, rather than letting
a typo fail silently.

### Optional health commands

A preset may carry a `health_command`, which answers whether a workload built
from that preset is serving:

```toml
[[hosts.presets]]
name = "api"
command = "exec cargo run --release"
health_command = "curl -fsS localhost:8080/healthz"
```

The probe runs in the workload's directory, on the workload's host, after each
liveness check, and it never enters the workload's own pane. Exit zero reads
`serving`; any other status reads `not serving` with the status.

Anything that stops the probe from reporting reads `health unknown`, which is
deliberately not a pass and not a failure: Tether states what it could not
establish rather than assuming the workload is fine or broken. That covers a
probe that timed out, could not be started, was killed by something outside
Tether, could not enter the directory, exited `126` or `127` because it could
not be executed, or ran over SSH to a host that could not be reached - for a
remote host, `255` is SSH's own transport failure rather than the workload's
answer, and it is read as such. A host that is unreachable is not probed at all;
its workloads report unknown once, rather than the refresh re-attempting a
connection the liveness check just proved dead.

All of a host's probes share one budget of at most five seconds, so a host's
status refresh costs the same whether it has one probed workload or fifty. A
probe that does not fit reports unknown.

A health command is trusted code on the same terms as `command`, and it is
subject to the same length limit. It runs only while a Tether surface is
refreshing status; nothing probes a workload on a schedule, and a result is
display-only - it is never written to `state.json` and never changes a
workload's lifecycle. A workload whose preset has no health command, or which
was created without a preset, reports nothing about serving rather than an
implied pass.

### Sharing a preset

Send the `[[hosts.presets]]` block. A preset does not name its own host, so where
it is pasted decides which machine the code runs on: TOML attaches the block to
the `[[hosts]]` entry above it, and a block dropped at the end of a file lands on
whichever host happens to be last.

A host entry Tether wrote already reads `presets = []`. Replace that line with the
block, under the host you mean:

```toml
[[hosts]]
name = "build"
target = "build.example.net"
roots = ["/srv/repos"]

[[hosts.presets]]
name = "tests"
command = "cargo test"
```

Adding the block while `presets = []` is still there is a duplicate key, and TOML
rejects the whole file rather than that one line. The same is true of a preset
whose name the host already uses, and of a field from a newer Tether that this one
does not know: adopting one block can leave the entire configuration unusable
until it is removed. `doctor` reports it as `unusable` and names the line.

Read what you are given the way you would read a shell script from the same
person. A preset is code: `command`, and `health_command` when there is one, each
run through `/bin/sh -c` on that host, as your login user there.

`herdr-tether host presets` prints every configured preset with the exact
`command`, the `health_command` when there is one, and the agent kind if it names
one, so reading what you were given does not mean opening the file. Every program
is indented under the preset that owns it and control characters are shown
escaped, because a command that arrived from somewhere else is where a terminal
escape sequence would arrive; `--json` is byte-exact for anything that needs the
original, and `--host NAME` narrows to one host.

The picker shows the same thing at the moment of choosing: highlighting a preset
on the command stage displays what it runs, and its probe when it has one, before
Enter creates anything. A command too long for the panel says how many characters
it left out rather than trailing off, so a long command is never mistaken for a
short one. `herdr-tether open --preset NAME` prints what the name resolved to
before the workload exists.

`herdr-tether host add NAME TARGET --preset NAME=COMMAND` avoids hand-editing when
the host is being added at the same time. It cannot carry `health_command` or
`herdr_agent`, which is why the TOML is the general answer.

### Why there is no import command

There is no `preset import`, no export format, and nothing that fetches. That is
a judgment about cost, not a claim that importing could not be done safely: a
command that printed the exact `command` and `health_command`, said which host
and directory they would bind to, and required typed confirmation would show a
recipient everything reading the block shows them, and would get the host binding
right rather than leaving it to where the paste landed.

The half of that which was worth having shipped without the import: `host presets`
prints the exact programs, and the picker and `open --preset` show them at the
moment they would run. What is missing is only the part that would take preset
text from elsewhere and adopt it.

It loses on what it costs. It is a permanent surface, a file format, and a second
way to add a preset, in exchange for a paste - and it would make preset text
something Tether itself takes from elsewhere and then executes. Tether's own
intake is otherwise deliberately narrow: hosts are ones you selected, Herdr
answers over a local socket, and session catalogues, `tmux` output, and SSH
aliases are validated, bounded, or display-only.

Configuration you did not write can already reach a shell, but through OpenSSH
rather than through Tether: an inherited `~/.ssh/config` can carry `ProxyCommand`
or `Match exec`, which is why `SECURITY.md` places OpenSSH configuration and
proxies inside the trusted computing base. A preset is the same kind of trust,
which is why that document places user-controlled presets there too. Keep write
access to the configuration as narrow as write access to `~/.profile` on every
host it names.

### Mission Control prompt capability

Each orchestration worker stores independent `observe_output`,
`open_interactive`, and optional `prompt_agent` booleans in `state.json`.
Missing `prompt_agent` defaults to `false` and is omitted when false, preserving
existing schema-v4 files. Use the native reviewed group editor or the optional
`orchestration add-worker --prompt-agent` adapter; do not hand-edit state.
For an exactly bound recognized agent, `observe_output` enables recent-output
read and semantic wait, while `open_interactive` enables focus/open.
`prompt_agent` is checked independently; neither of the other capabilities can
authorize input.

The grant is inert unless Herdr recognizes the worker's explicit
`herdr_agent` kind and every live ownership, membership, pane-binding, and
settled-state check succeeds. Prompt text and delivery results are never stored
in `state.json` or `agent-view.json`.

### Agent sidebar preference

The group action screen stores one optional orchestration-group ID and one of three view modes in `agent-view.json` beside
`state.json`: all group agents, agents needing attention, or remote group
agents. Herdr plugin actions use the same authoritative
`HERDR_PLUGIN_STATE_DIR`; standalone defaults use the path shown above. The
file is created lazily with the same private permissions, advisory locking, and
atomic replacement as other Tether state.

The preference is presentation-only. It contains no command, terminal output,
agent state, credential, host, or path. Tether restores its source-owned view
after Herdr startup or live handoff, and **Restore default Agents sidebar**
clears only Tether's view. Do not hand-edit this file; use those group actions
so persisted preference and Herdr's current view change transactionally.

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

### Notifications

`notifications.agent_blocked` and `notifications.agent_done` ask Herdr to show a
toast when a Mission Control agent changes into `BLOCKED` or `DONE`.
`notifications.workload_failed` does the same when a workload's command ends with
a failing status. All three default to true. A toast is sent only on a change
into that state, so an agent that stays blocked, or a workload that stays failed,
does not notify again on every refresh.

A setting that a configuration file predates falls back to these defaults rather
than to off, so an older file keeps behaving as documented.

These are advisory and best effort. Herdr shows nothing unless you have enabled
its own `ui.toast.delivery` setting, which ships as `off`, and it may decline
while another surface owns the screen. Tether treats either outcome as normal:
the Mission Control tile remains the authoritative view.

Notification text carries a short workload reference and either the agent's state
or the exit status `tmux` reported. It is deliberately not the tile's display
title, because that title is generated from the workload's host, repository name,
and preset, and a notification leaves the surface that produced it. Host,
directory, command, capture, and prompt text are never included. Tether notifies
only while a Mission Control surface is open; nothing observes a workload or an
agent when no Tether surface is running.

### Retention

`retention.closed_days` controls when safely finalized ended history becomes eligible for metadata-only cleanup. The default is 30 days and the value must be positive. Cleanup does not contact a host, invoke `tmux`, or stop a workload.

## State is not configuration

State records Tether-owned identity, resolved host target, directory, launch information, optional explicit agent hint, lifecycle status, timestamps, and orchestration groups. It does not store SSH passwords, private keys, access tokens, terminal contents, or telemetry identifiers.

Do not hand-edit `state.json` or `agent-view.json`. Lifecycle transitions and Agent view preference changes are lock-protected and validated so interrupted work can be reconciled safely. Supported older schemas migrate atomically; a corrupt or future schema fails closed.

For the meaning of each state, see [Lifecycle](lifecycle.md). For storage and trust boundaries, see [Architecture and security](architecture.md).
