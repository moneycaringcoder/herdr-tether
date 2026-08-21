# Troubleshooting

Start with the installation and context report:

```sh
herdr-tether doctor
```

Errors in the Tether picker stay visible until `Enter` retries the same operation or `Esc` cancels it. Read the named operation and safe next action before retrying.

## `prefix+t` does nothing

1. Confirm the plugin and actions are installed:

   ```sh
   herdr plugin action list --plugin moneycaringcoder.tether
   ```

2. Run the one-time setup action:

   ```sh
   herdr plugin action invoke moneycaringcoder.tether.setup
   ```

3. If setup reports a conflict, inspect your Herdr configuration and choose which `prefix+t` binding should own the key. Tether refuses to overwrite another action.
4. Restart or reload Herdr if its configuration reload was interrupted.

Setup keeps an exact sibling backup before changing the Herdr configuration. While the plugin is installed, `herdr plugin action invoke moneycaringcoder.tether.rollback` restores and consumes that backup only when the current configuration still matches Tether's installed version; check the reported action log for success. The synchronous standalone equivalent is `herdr-tether setup keybinding --rollback`. Both refuse to overwrite later edits.

## A remote host is unreachable

Check the same non-interactive path Tether depends on:

```sh
ssh -o BatchMode=yes -- build.example.net 'tmux -V'
herdr-tether host check build
```

Use your actual configured target in place of `build.example.net`. Fix DNS, network, SSH agent, proxy, host key, authorization, or remote `tmux` installation outside Tether, then press `r` to retry.

Never work around this with global `StrictHostKeyChecking=no`. Tether deliberately preserves OpenSSH's host-key policy.

Tether uses bounded OpenSSH connection setup and reuses a validated master for
60 seconds through `ControlMaster=auto`, `ControlPersist=60`, and a `%C` socket
under `~/.ssh`. This reduces repeated remote discovery and observation setup
without bypassing your SSH configuration. If multiplexing fails, confirm that
`~/.ssh` exists, is private, and is writable by the current user.

## A workload is shown as unreachable

Unreachable means Tether cannot prove the exact workload state. It is not evidence that the process ended. Tether therefore disables Open, Stop, Restart, and Remove when those actions would require a state guess.

1. Restore access to the selected local or remote `tmux` server.
2. Press `r` to retry observation.
3. If access is healthy but inspection still fails, run `herdr-tether doctor` and verify `tmux` is version 3.3 or newer.

Do not delete state or kill a similarly named session to force the display to change.

## A command finished but the record remains

A completed command should appear as **Ended**, with exit context when available. Press `Enter` to restart it in the original directory, or `x` to remove its retained record.

If the row is **Unreachable** instead, restore backend access and retry; Tether will not turn an unknown observation into Ended. If it remains contradictory after a successful refresh, capture sanitized version and state information for a bug report without including command bodies, hostnames, directories, or terminal output.

## Stop or Restart was interrupted

Tether persists recoverable transitions before destructive or identity-changing work. Reopen the picker and follow the visible retry or cancel action:

- **Stopping** does not masquerade as safely Running or Ended. Retry reconciliation against the exact owned identity.
- A failed Restart does not attach to the ended identity or silently duplicate a running workload.
- If placement fails, Tether preserves the invoking pane and reports the safe next action.

Never kill a broad `tmux` name prefix. Tether lifecycle operations target exact owned identities.

## A repository is not discovered

- Confirm the directory is beneath a configured local or host root.
- Increase the relevant discovery bound deliberately if the tree is deeper or larger than the current limit.
- Symlinked directories are not followed.
- A recent directory is only a picker suggestion; it does not become a scan root.
- Literal aliases are discovered from bounded `Include` files beneath the
  primary `~/.ssh` directory. Wildcard `Host` patterns, include cycles, and
  includes that resolve outside that directory are deliberately ignored.

You can still enter a literal existing path from the directory stage.

## Placement or Replace current pane fails

Tether creates a destination and verifies that the attach process started before closing a replaced source pane. Dispatch or readiness failures clean up the destination when possible and preserve the source. A source-close failure leaves the verified destination running and identifies both panes.

When foreground processes are present, replacement requires interactive confirmation. Use split right, split down, or new tab for a non-destructive alternative.

GUI applications may provide a reduced `PATH`. Tether checks common system and Homebrew locations for `ssh` and `tmux`, but `herdr-tether doctor` should still be used to verify the actual environment.

## A workload is absent from Herdr's Agents sidebar

Start with `herdr --version` and confirm it reports 0.8.0 or newer.

The workload must run a Herdr-supported agent and must have been created with
an explicit `herdr_agent` preset value or `--herdr-agent KIND`. Tether never
guesses a kind from the command. Changing a preset later does not rewrite an
existing durable workload record. A workload with no hint keeps Tether's
pane/session title and picker state instead of a sidebar agent row.

If a Tether Agents sidebar view is active, it intentionally hides agents
outside the selected group, status, or remote-origin filter. Open an applicable
member from Tether or Mission Control after activating the view so Tether can
label that pane for the selected group and remote origin. Use **Restore default
Agents sidebar** to remove Tether's filter.

If applying or clearing the view fails, the prior preference is preserved.
Confirm the plugin is still enabled, then retry from the group action screen. Tether's Agent view is presentation-only and never changes
workload lifecycle or ownership.

## Mission Control cannot prompt an agent

Start with `herdr --version`. Agent control requires Herdr 0.8.0 or newer and
a reachable Herdr session; without one, Tether keeps ordinary Observer and
workload management and says so instead.

The worker must be running and exact-owned, have an explicit `herdr_agent`
preset value or `--herdr-agent KIND`, and have prompt permission granted with
`p` in the reviewed group topology. Open that group member from Mission Control
so Tether can report its bounded group, session, membership, and remote-origin
metadata to Herdr.

Prompt is allowed only while Herdr resolves exactly one matching recognized
agent in `IDLE` or `DONE`. `WORKING`, `BLOCKED`, `UNKNOWN`, `UNREACHABLE`, and
`STALE` deliberately reject input. A verified pane move remains valid when the
same recognized occupant moved; a replacement, unverified move, stale
membership, or ambiguous binding rejects delivery. Press `r` to
retry/resnapshot rather than bypassing the binding check.

`REJECTED` means Herdr confirmed no delivery. `UNCERTAIN` means the connection
failed or timed out after delivery may have begun. Tether never retries an
uncertain prompt automatically. Inspect the agent state/output before deciding
whether to submit a new reviewed prompt.

An SSH or Herdr connection loss preserves the last-known tile as `STALE`.
Reconnect and press `r`; Tether will resubscribe and resnapshot. Closing Mission
Control while disconnected still leaves every Tether workload running.

`UNREACHABLE` and `STALE` both mean the tile is not current, and each says which
it is and what to do about it on the tile itself, so this page is a reference
rather than a requirement.

`STALE` has three reasons, and the tile names the one that applies. Output that
could not be re-read is shown as retained, with the time it was last live.
A lost Mission Control connection shows the last known state, also dated, and
comes back on its own once the connection does. A binding that is no longer
exactly one recognized occupant is different: `r` resnapshots, but if the pane
really moved or was replaced, the worker has to be reopened from the picker
rather than worked around.

`UNREACHABLE` means nothing answered. When there is nothing retained the tile
says so, because there is no remembered output to read; when Herdr is
unreachable but `tmux` capture still worked, the tile shows that output and dates
it as not current rather than claiming it is live. The remedy is access to Herdr
or the host first.

In the picker, a host being rechecked keeps whatever its last answer was: if that
answer was good, the footer says the wait is the remedy, and if the host was
already failing it keeps naming the failure instead of implying a fix is in
progress. A host whose check failed says why open, restart, and stop are
withheld, using the same word the row shows - `offline`, `timeout`, or `error`.

## The picker is difficult to read or use with assistive technology

Use a terminal at least 80 columns wide when possible. Tether wraps controls on
narrower terminals, but long host and directory labels can still reduce
readability. Every state and action has a text label; color is supplementary.

The interactive picker is a terminal canvas and does not claim screen-reader
support. Use `herdr-tether snapshot --pretty` for a selectable, non-interactive
JSON view with explicit text statuses. The picker itself is keyboard-operable:
arrow keys or `Tab`/`Shift+Tab` move, `Enter` chooses the printed action, and
`Esc` backs out.

## Collecting a safe report

Include:

- Tether, Herdr, operating-system, OpenSSH, and `tmux` versions;
- the operation that failed and the exact sanitized error category;
- whether the target was local or remote; and
- a minimal reproduction using disposable paths and commands.

Exclude credentials, SSH destinations, usernames, private paths, repository names, command bodies, terminal output, state files, and backups. Report vulnerabilities through the [security policy](../SECURITY.md). Use a public issue only for non-sensitive defects.

See [Architecture and security](architecture.md) for component boundaries and [Configuration](configuration.md) for path and schema details.
