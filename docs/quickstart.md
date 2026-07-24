# Quickstart

This guide installs Tether, adds the `prefix+t` launcher, and creates a first durable workload. For the mental model behind each action, read [Lifecycle](lifecycle.md).

## 1. Check requirements

You need:

- macOS or Linux;
- Herdr 0.7.3 or newer;
- `tmux` 3.3 or newer locally and on each remote host;
- Git and Rust 1.88 or newer; and
- OpenSSH when using remote hosts.

Remote access must already succeed non-interactively under your normal SSH keys, agent, proxy, and `known_hosts` policy. Tether does not configure SSH trust.

## 2. Install the plugin

Choose one source:

**Stable v0.5.0.** Install the release tag:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.5.0
```

**Development (`main`).** Use this to follow the public development line, not
a release:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref main
```

**Immutable reviewed commit.** Replace the value below with the full commit SHA
you reviewed:

```sh
TETHER_REV=FULL_COMMIT_SHA_YOU_REVIEWED
herdr plugin install moneycaringcoder/herdr-tether --ref "$TETHER_REV"
```

Herdr previews the source and build commands before confirmation. Review them,
then verify the installed action list:

```sh
herdr plugin action list --plugin moneycaringcoder.tether
```

Herdr keeps the plugin binary in its managed checkout; it does not add `herdr-tether` to your shell's `PATH`.

## 3. Install the launcher

Run the setup action once:

```sh
herdr plugin action invoke moneycaringcoder.tether.setup
```

The action initializes Tether's private configuration and state, adds `prefix+t` for `moneycaringcoder.tether.open`, and asks Herdr to reload its configuration. It:

- makes no change when the same binding already exists;
- refuses a conflicting `prefix+t` binding;
- refuses invalid or unmergeable configuration; and
- saves the exact prior configuration to a sibling backup before changing it.

The backup is consumed when rollback succeeds. Rollback also refuses to
overwrite configuration changed after setup. While the plugin is still
installed, invoke its rollback action and check the reported action log for
success:

```sh
herdr plugin action invoke moneycaringcoder.tether.rollback
```

If the standalone CLI is installed, the equivalent synchronous command is
`herdr-tether setup keybinding --rollback`.

Herdr 0.7.3 does not expose a generic plugin-action menu, so this explicit one-time setup action is required. Normal plugin install and open actions do not silently change the keybinding.

## 4. Create and leave a workload

1. Press your Herdr prefix, then `t`.
2. Choose **Local**.
3. Choose **Create new Tether workload**.
4. Select a repository or enter an existing directory.
5. Choose a command and placement.
6. Close the Herdr view when you are done viewing it.

Closing the view leaves the workload running. Open Tether again and press `Enter` on that running workload to reconnect.

If the command is a Herdr-supported agent hidden behind `tmux` or SSH, create
the workload from a preset with an explicit `herdr_agent` or use the standalone
`open --herdr-agent KIND` option. Tether never infers a kind from a command.
See [Agent hints in Configuration](configuration.md#host-entries-and-presets).

## 5. Add a remote host (optional)

Install the standalone CLI when you want administration commands in your shell.
The stable release is:

```sh
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.5.0 --locked herdr-tether
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

Herdr passes authoritative plugin directories to plugin actions. To have standalone commands edit those same stores, export the corresponding directories first:

```sh
export HERDR_PLUGIN_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/herdr/plugins/config/moneycaringcoder.tether"
export HERDR_PLUGIN_STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/moneycaringcoder.tether"
herdr-tether setup --yes
```

Without these variables, the standalone CLI intentionally uses its separate default paths documented in [Configuration](configuration.md).

Add and verify a target:

```sh
herdr-tether host add build build.example.net --root /srv/repos
herdr-tether host check build
```

Tether also discovers literal aliases from bounded, cycle-safe `Include` files
beneath the primary `~/.ssh` directory. Wildcard `Host` patterns and includes
that escape that directory are ignored; an included-only literal alias needs no
duplicate configuration.

## 6. Create a group and open Mission Control

Orchestration groups and the read-only Observer work on Herdr 0.7.3+. Herdr
0.7.5+ additionally unlocks event-driven Mission Control agent actions.

From Herdr, press `prefix+t`, then press `o` for **Observers**:

1. Press `n` for **Create Observer**.
2. Choose one running workload as `ORCHESTRATOR`.
3. Select one or more `WORKER` workloads with `Space`.
4. Optional on Herdr 0.7.5+: press `p` on a selected worker to grant prompt
   permission. The workload must have been created with an explicit
   `herdr_agent` preset value or `--herdr-agent KIND`.
5. Press `Enter`, review every role and permission, then press `Enter` again.
6. Select the group and choose **Open Mission Control**. Older supported Herdr
   versions show **Open Observer** plus the 0.7.5 upgrade requirement.

The default worker capabilities remain bounded output observation and
interactive open; prompt permission is separate, explicit, and defaults off.
Group changes mutate metadata only. Up to 32 groups, 64 workers per group, four
visible workers per page, and eight prompt destinations are accepted.

Mission Control keys:

- arrows select; `Tab`/`Shift+Tab`, Page Up/Page Down, or `[`/`]` change page;
- `Enter` opens the selected durable terminal without replacing Mission Control;
- `Space` selects prompt destinations and `p` begins prompt review;
- `f` focuses the current Herdr agent, `v` reads recent output, and `w` waits
  briefly for `IDLE`, `DONE`, or `BLOCKED`;
- `r` retries/resnapshots; `q`, `Esc`, or `Ctrl+C` closes only the view.

The footer shows only controls available to the group: focus/open requires
`open_interactive`, read/wait requires `observe_output`, and prompt selection
requires the separate `prompt_agent` grant.

Before prompting, Tether shows the exact destinations and prompt. Type `SEND`
to deliver once or press Enter to cancel. Only current exact-owned, running,
prompt-authorized `IDLE` or `DONE` agents receive input. Per-target
`REJECTED` and `UNCERTAIN` results are not treated as success, and uncertain
requests are never retried automatically. Tether does not persist prompt text.

Tiles report explicit agent states. Herdr events update attached agents without
recurring SSH capture; detached/non-agent workloads retain bounded `tmux`
fallback. A connection loss retains last-known information as `STALE`, disables
input, and never fabricates completion. Closing the view never stops a workload.

Use **Edit workers** or **Change orchestrator** to update a group after another
explicit review. Press `d` to delete group metadata only. The Agents sidebar
actions can show the whole group, only `BLOCKED`/`DONE` agents needing
attention, or only remote group agents. Each installs a source-owned Herdr
0.7.5+ filter used for exact pane binding; **Restore default Agents sidebar**
clears it.

The standalone `herdr-tether orchestration` commands remain an optional adapter
API. Native users do not need exact IDs, shell commands, or exported Herdr
environment variables. Placement still requires a Herdr pane and normalizes
`replace-current-pane` to `split-right` so the source remains available.

## Update or remove Tether

Reinstall the desired reviewed revision to update both managed checkout and
build. Herdr previews it again before confirmation:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref main
```

Before installing an older version, back up Tether's configuration and state. A newer schema may not be understood by older code.

Stop any workloads you do not want to keep. Then remove the launcher while its
rollback action is still installed, check the reported action log for success,
and uninstall:

```sh
herdr plugin action invoke moneycaringcoder.tether.rollback
herdr plugin uninstall moneycaringcoder.tether
```

If installed, remove the standalone binary with `cargo uninstall herdr-tether`.

Uninstalling does not intentionally stop workloads and does not remove
configuration or state. Running workloads continue under `tmux`; retained data
must be removed separately if desired.

## Next steps

- [Lifecycle](lifecycle.md) explains leave, stop, restart, remove, and unreachable states.
- [Configuration](configuration.md) covers hosts, presets, roots, placement, and retention.
- [Troubleshooting](troubleshooting.md) covers installation, SSH, and workload recovery.
