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
- Tether does not traverse OpenSSH `Include` directives to discover aliases. Add an included-only alias explicitly.

You can still enter a literal existing path from the directory stage.

## Placement or Replace current pane fails

Tether creates a destination and verifies that the attach process started before closing a replaced source pane. Dispatch or readiness failures clean up the destination when possible and preserve the source. A source-close failure leaves the verified destination running and identifies both panes.

When foreground processes are present, replacement requires interactive confirmation. Use split right, split down, or new tab for a non-destructive alternative.

GUI applications may provide a reduced `PATH`. Tether checks common system and Homebrew locations for `ssh` and `tmux`, but `herdr-tether doctor` should still be used to verify the actual environment.

## A workload is absent from Herdr's Agents sidebar

Start with `herdr --version`.

- On Herdr 0.7.3 and 0.7.4, arbitrary workloads hidden behind SSH and `tmux`
  cannot be registered as sidebar agents. Use Tether's pane/session title and
  picker state.
- On Herdr 0.7.5 and newer, the workload must run a Herdr-supported agent and
  must have been created with an explicit `herdr_agent` preset value or
  `--herdr-agent KIND`. Tether never guesses a kind from the command. Changing
  a preset later does not rewrite an existing durable workload record.

If **Show group in Agents sidebar** is active, the filter intentionally hides
unrelated agents. Open an applicable member from Tether or Observer after
activating the view so Tether can label that pane for the selected group. Use
**Restore default Agents sidebar** to remove Tether's filter.

If applying or clearing the view fails, the prior preference is preserved.
Confirm the plugin is still enabled on Herdr 0.7.5 or newer, then retry from the
group action screen. Tether's Agent view is presentation-only and never changes
workload lifecycle or ownership.

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
