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

**Stable v0.4.1.** Install the release tag:

```sh
herdr plugin install moneycaringcoder/herdr-tether --ref v0.4.1
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
  --tag v0.4.1 --locked herdr-tether
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

Tether also discovers literal aliases in the primary `~/.ssh/config`. It does not traverse `Include` directives for alias discovery; add an included-only alias explicitly.

## 6. Create an Observer group

Orchestration groups and Observer are included in stable v0.4.1.

From Herdr, press `prefix+t`, then press `o` for **Observers**:

1. Press `n` for **Create Observer**.
2. Choose one running workload as `ORCHESTRATOR`.
3. Select one or more `WORKER` workloads with `Space`, then press `Enter`.
4. Review the topology and press `Enter` again to create it.
5. Select the new group and choose **Open Observer**.

Tether derives bounded labels from safe workload metadata. New workers receive
the safe default capabilities: bounded read-only output observation and
interactive open. Group creation does not create, adopt, stop, restart, remove,
or send input to a workload. Up to 32 groups and 64 workers per group are
accepted.

One outer pane renders one worker full-size, two side by side, or three to four
in a 2×2 grid. Additional workers appear on deterministic four-worker pages.
Use arrows to select, Page Up/Page Down, `Tab`/`Shift+Tab`, or `[`/`]` to page;
use `r` to refresh and `Enter` to open an authorized running worker as a normal
Tether view. Press `q`, `Esc`, or `Ctrl+C` to leave Observer. Membership and
lifecycle labels refresh while it runs; worker input is never forwarded.

To change a group, choose **Edit workers** or **Change orchestrator**, make the
selection, review the resulting topology, and confirm. Promoting a worker to
orchestrator removes it from the worker list. To remove a group, press `d` and
confirm. All operations change only group metadata and leave every workload and
pane alone.

With Herdr 0.7.5 or newer, the same action screen offers **Show group in Agents
sidebar**. The view contains only panes Herdr recognizes as agents. After
activating it, open each applicable group member from Tether or Observer;
Tether labels that pane for the selected group. Agent commands hidden behind
`tmux` or SSH need the explicit hint described above.

The view is explicit and reversible: it persists across Herdr startup or live
handoff and does not open or mutate workloads by itself. Choose **Restore
default Agents sidebar** to clear Tether's filter. Herdr 0.7.3 and 0.7.4
continue to support ordinary Tether and Observer workflows without this
optional view.

The standalone `herdr-tether orchestration` commands remain available as an
optional machine/adapter API. Native picker users do not need exact IDs, shell
commands, a separate CLI installation, or exported Herdr environment variables.
Opening Observer still occurs at Herdr's plugin placement boundary; Tether
normalizes `replace-current-pane` to `split-right` so the source pane remains
available.

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
