<p align="center">
  <img src="assets/tether-wordmark.svg" width="560" alt="Tether for Herdr">
</p>

<p align="center"><strong>Keep local and remote terminal workloads running when the view goes away.</strong></p>

<p align="center">
  <a href="https://github.com/moneycaringcoder/herdr-tether/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/moneycaringcoder/herdr-tether/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-0f766e.svg"></a>
  <a href="https://github.com/ogulcancelik/herdr"><img alt="Herdr 0.7.3 or newer" src="https://img.shields.io/badge/Herdr-0.7.3%2B-172033.svg"></a>
  <img alt="Platforms: macOS and Linux" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-475569.svg">
</p>

Tether gives [Herdr](https://github.com/ogulcancelik/herdr) a safe, focused way to create, leave, reopen, stop, and restart durable terminal work. It handles ordinary local or SSH-backed `tmux` workloads; Herdr keeps owning the terminal surface.

> **The lifecycle in one sentence:** Closing the view leaves it running; **Stop** ends it; **Enter** brings it back.

## Why Tether?

An editor, build, test watcher, or long-running command should not end because its Herdr pane closed or an SSH connection dropped. Tether remembers the exact workload, host, repository directory, and launch command while keeping ownership boundaries explicit:

- one `prefix+t` workflow for local and remote work;
- reconnect without hunting for session names;
- visible running, ended, and unreachable states;
- exact destructive actions only for Tether-owned workloads;
- attach-only discovery for existing external `tmux` sessions.

Tether does not stream a remote Herdr workspace or federate Herdr instances. It uses OpenSSH and `tmux` on the selected machine.

## Install

### Requirements

- macOS or Linux
- [Herdr](https://github.com/ogulcancelik/herdr) 0.7.3 or newer
- `tmux` 3.3 or newer locally and on remote hosts
- Git and Rust 1.88 or newer
- OpenSSH with non-interactive access already configured for remote hosts

Choose the source that matches your stability and reproducibility needs. Herdr
shows the source and build commands for review before you confirm.

**Stable v0.4.0.** Install the release tag:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.4.0
```

**Development (`main`).** This follows the public development line, not a
release:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref main
```

**Immutable reviewed commit.** Replace the value with the full commit SHA you
reviewed; unlike a branch, that revision cannot move:

```sh
TETHER_REV=FULL_COMMIT_SHA_YOU_REVIEWED
herdr plugin install moneycaringcoder/herdr-tether --ref "$TETHER_REV"
```

Whichever source you choose, run the setup action once:

```sh
herdr plugin action invoke moneycaringcoder.tether.setup
```

The setup action safely adds the `prefix+t` launcher and reloads Herdr's
configuration. Setup first checks the required runtime tools—Herdr, `tmux`, and
OpenSSH—before writing Tether or Herdr files. Setup
refuses a conflicting binding and keeps an exact backup for rollback. After
setup, press your Herdr prefix followed by `t`.

Want the standalone administration commands too? The stable install is:

```sh
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.4.0 --locked herdr-tether
```

For development `main`:

```sh
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --branch main --locked herdr-tether
```

For an immutable reviewed commit:

```sh
TETHER_REV=FULL_COMMIT_SHA_YOU_REVIEWED
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --rev "$TETHER_REV" --locked herdr-tether
```

Then run `herdr-tether doctor`. For automation and support bundles, `herdr-tether doctor --json` emits a versioned, bounded report with typed check statuses and redacted diagnostics.

Maintainers can validate the packaged public surface with:

```sh
python3 tools/check_docs.py
python3 tools/check_package.py
python3 tools/check_public_contracts.py --archive target/package/herdr-tether-VERSION.crate --binary target/release/herdr-tether
```

See the [quickstart](docs/quickstart.md) for plugin data paths, keybinding rollback, updates, and uninstalling.

### Optional Hermes skill

Hermes users can install Tether's optional, runtime-independent reference guide
directly from this repository.

**Stable v0.4.0:**

```sh
hermes skills install https://raw.githubusercontent.com/moneycaringcoder/herdr-tether/v0.4.0/integrations/hermes/SKILL.md
```

**Development (`main`):**

```sh
hermes skills install https://raw.githubusercontent.com/moneycaringcoder/herdr-tether/main/integrations/hermes/SKILL.md
```

**Immutable reviewed commit:**

```sh
TETHER_SKILL_REF=FULL_COMMIT_SHA_YOU_REVIEWED
hermes skills install "https://raw.githubusercontent.com/moneycaringcoder/herdr-tether/${TETHER_SKILL_REF}/integrations/hermes/SKILL.md"
```

The [skill source](integrations/hermes/SKILL.md) describes Tether's public
snapshot, lifecycle, and development orchestration commands. Hermes is a
reference adapter, not a Tether runtime dependency.

## The `prefix+t` workflow

1. Press `prefix+t` in Herdr.
2. Choose **Local** or a configured SSH host.
3. Open a running workload, restart an ended one, attach to an external session, or choose **Create new Tether workload**.
4. For a new workload, choose a repository, command, and placement.
5. Close the Herdr view whenever you want. The workload remains available.

On development `main`, when Herdr supplies an optional pane or workspace
working directory, Tether brings an already-known matching workload and host to
the front. The context is only a preference: malformed, unknown, or ambiguous
paths change nothing, and Tether never creates a host, path, command, or
selection from it.

The picker keeps actions contextual:

| Workload state | Primary action | Destructive action |
| --- | --- | --- |
| **Running** | `Enter` **Open** | `x` **Stop**, with confirmation |
| **Ended** | `Enter` **Restart** | `x` **Remove** |
| **Unreachable** | `r` **Retry** | None until Tether can verify safety |
| **External** | `Enter` **Open** | None—external sessions are never owned |

`Esc` backs out or closes Tether. See [Lifecycle](docs/lifecycle.md) for interrupted operations, cleanup, and ownership details.

When a workload command runs a Herdr-supported agent behind Tether's `tmux` or
SSH boundary, configure an explicit agent hint rather than relying on process
guessing. Presets accept `herdr_agent = "codex"` in `config.toml`; ad hoc CLI
creation accepts `--herdr-agent codex`. On Herdr 0.7.5 and newer, the resulting
view is recognized as that agent while retaining Tether's durable lifecycle.

## Orchestration and Observer

The opt-in orchestration-group and Observer workflow organizes existing
Tether-owned workloads for observation; it does not launch, adopt, stop,
restart, or remove them.

Use the native workflow from the ordinary `prefix+t` picker:

1. Press `o` to open **Observers**.
2. Press `n` to create a group, choose one running workload as orchestrator,
   select one or more running workers with `Space`, then press `Enter`.
3. Review the explicit `ORCHESTRATOR` and `WORKER` topology, then press `Enter`
   again to create it. Ambiguous safe labels receive stable bounded references.
4. Select the group and choose **Open Observer**. The source launcher remains
   available while one companion pane opens.

Existing groups can change orchestrator, edit workers, or be deleted from the
same screen. Every role change has a topology review; metadata edits are
reversible, and confirmed deletion does not touch workloads or panes. Ordinary
users never need session IDs,
shell commands, a standalone Tether installation, or environment setup.

On Herdr 0.7.5 and newer, a group can also become an opt-in native Agents
sidebar view. Select **Show group in Agents sidebar** from its action screen;
Tether filters the sidebar to recognized agents in that group and restores the
selection after Herdr startup or live handoff. Opening a group member labels
that pane for the view. **Restore default Agents sidebar** removes only
Tether's filter. This feature never launches, adopts, or sends input to a
workload, and Herdr 0.7.3/0.7.4 remain supported without it.

The CLI remains an optional machine/adapter API:

```sh
herdr-tether orchestration create GROUP --title TITLE --orchestrator SESSION
herdr-tether orchestration add-worker GROUP SESSION --title TITLE \
  --observe-output --open-interactive
herdr-tether orchestration list --json
herdr-tether orchestration observe GROUP --placement split-right
```

Observer renders at most four worker captures per page: one worker fills the
page, two split side by side, and three or four use a 2×2 grid. More workers use
deterministic pages. Tiles distinguish loading, ready-but-empty output, and
unavailable output. A recoverable refresh failure keeps the last authorized
tiles, page, and selection visible with retry guidance until refresh succeeds.
An empty group offers only refresh and back; output is never forwarded as
worker input.

Opening Observer requires the current Herdr plugin context. The native picker
already supplies it. A standalone adapter nested inside a Tether `tmux` workload
must explicitly hand the launch back to a Herdr pane. Tether normalizes
`replace-current-pane` to `split-right` so the invoking pane remains available.

## Terminal and accessibility notes

Tether is keyboard-operable: use the arrow keys or `Tab`/`Shift+Tab` to move,
`Enter` to choose the action printed for the selected row, and `Esc` to back out.
Held navigation keys repeat without wrapping, while refresh and open remain
single-shot. Long picker and Observer-manager lists show position and
more-above/more-below guidance; panes that cannot fit actionable content show
one resize message. Captured Unicode is truncated only at extended grapheme
boundaries, so flags, keycaps, combining sequences, and emoji remain intact
within the documented byte, line, and display-cell limits.
Actions and states are written as text and never conveyed by color alone.

For the most readable picker, use a terminal at least 80 columns wide. The layout
wraps at narrower widths, but long host or directory labels leave less room for
the action and status. Tether does not claim screen-reader support for its
interactive terminal canvas. For a non-interactive, selectable text view, run
`herdr-tether snapshot --pretty`; its JSON uses explicit status names rather than
color or cursor position.

## Local and remote examples

### Start locally

Open Tether, choose **Local**, select a discovered repository, and start a shell or configured preset. Closing its Herdr view only detaches that view. Open Tether again and press `Enter` on the running workload to reconnect.

A fully specified standalone request can skip the picker:

```sh
herdr-tether open --directory /workspace/project --command 'cargo test' \
  --placement split-right
```

`/workspace/project` is a documentation example; use an existing directory on your machine.

### Add a remote host

Tether can use a literal alias from your primary `~/.ssh/config`, or you can add an explicit target:

```sh
herdr-tether host add build build.example.net \
  --root /srv/repos \
  --preset 'editor=exec vi'
herdr-tether host check build
```

Then press `prefix+t`, choose **build**, and use the same create/open/leave workflow. Tether does not copy keys or passwords; OpenSSH continues to own authentication, proxies, and host-key verification.

### Inspect without attaching

```sh
herdr-tether snapshot --pretty
```

The snapshot reports bounded host, repository, owned-workload, and safe external-session data. Partial or unreachable results stay explicit. It does not include preset command bodies, terminal output, raw backend errors, credentials, or private storage paths.

## Safety and limitations

- **Only Tether-owned workloads can be stopped or removed.** Discovered external `tmux` sessions are attach-only.
- **Unknown is not dead.** When a host or backend cannot be inspected, Tether does not infer that destructive action is safe.
- **Closing a view is non-destructive.** Stop is separate, explicit, and confirmed.
- **Persisted and status inputs are bounded.** Over-budget state, configuration, and status requests fail with actionable category-level errors instead of being truncated or expanding follow-on work.
- **Discovery has one request-wide budget.** It stops at its global result, deadline, or cancellation boundary and reports the completion reason deterministically.
- **Lifecycle recovery is idempotent.** Metadata removal also reconciles Observer groups atomically; if exact Stop succeeds but final persistence fails, retry finalizes the existing stopping transition without another kill.
- **Atomic writes retain existing modes.** Replacing an existing regular configuration or state file preserves its permission bits; newly created private files use mode `0600`.
- **Commands and presets are trusted code.** They run through the selected machine's shell; Tether is not a sandbox.
- **SSH policy remains yours.** Tether uses `BatchMode=yes` and never weakens host-key verification.
- **Remote support is SSH + tmux.** Tether does not provide remote Herdr federation or pane streaming.
- **Herdr 0.7.3 sidebar limitation:** Herdr has no general API for registering arbitrary nested workloads as sidebar agents. Tether uses clear pane/session titles and does not fabricate agent state.
- Tether reads literal aliases from the primary `~/.ssh/config`; it does not traverse `Include` directives for discovery. Explicitly add an alias that is only present in an included file.

Read the [architecture and security boundaries](docs/architecture.md) before deploying Tether in a sensitive environment. Vulnerabilities should be reported through the [security policy](SECURITY.md), not a public issue.

## Documentation

| Guide | Start here when… |
| --- | --- |
| [Quickstart](docs/quickstart.md) | installing, configuring the launcher, or creating the first workload |
| [Lifecycle](docs/lifecycle.md) | deciding whether to leave, stop, restart, remove, or retry |
| [Configuration](docs/configuration.md) | adding hosts, roots, presets, placement, or retention settings |
| [Troubleshooting](docs/troubleshooting.md) | a host, workload, install, or pane action did not behave as expected |
| [Architecture and security](docs/architecture.md) | reviewing trust boundaries, persistence, transport, or ownership |
| [Contributing](CONTRIBUTING.md) | developing or testing Tether |
| [Security policy](SECURITY.md) | reporting a vulnerability |

## License

Tether is [MIT licensed](LICENSE). Its [visual assets](assets/README.md) are original project artwork distributed under the same license.
