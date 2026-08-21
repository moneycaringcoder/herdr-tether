# Workload lifecycle

Tether separates a workload from the Herdr view that displays it.

> Closing the view leaves it running; **Stop** ends it; **Enter** brings it back.

This distinction is the main safety rule. A pane, tab, overlay, or SSH client is only a view. The Tether-owned workload has its own durable identity.

## Actions people take

### Leave

Close or detach the Herdr view. Tether does not ask for confirmation because leaving is non-destructive. The workload continues in its owned `tmux` session, even if the SSH connection carrying the view drops.

Open Tether later and press `Enter` on the running workload to reconnect.

### Stop

Press `x` on a running Tether-owned workload, then `Enter` to confirm **Stop** or `Esc` to cancel. Tether re-inspects the exact owned identity before ending it. It never treats cached display status as permission to kill a session.

Stop is unavailable for external sessions. If the host or backend cannot be inspected safely, the workload becomes unreachable instead of being assumed dead.

### Restart

A workload is **Ended** when its command exits normally, is interrupted, crashes, or otherwise completes. It is not offered as a running attachment.

Press `Enter` on an ended workload to choose **Restart**. Tether recreates its original command in its original directory, using a safe identity transition. A failed restart remains recoverable and does not turn an unverified workload into a running one.

A command that exits with a failing status within ten seconds of starting will do the same thing if it is restarted unchanged, which is the loop worth interrupting. Tether names that end apart from an ordinary failure and paces the next Restart for thirty seconds from it, saying in the picker how long is left and that it never restarts anything itself. `herdr-tether session restart` declines for the same window and names the wait. Remove stays available throughout, and nothing about the pace changes what a restart does when it runs: waiting is the whole of it.

The pace comes from the last end alone, not from a tally of previous ones. A workload that fails immediately, is restarted after the wait, and fails immediately again is paced again, because each end is judged on its own; Tether keeps no history of the incarnation a restart replaced.

### Remove

Press `x` on an ended workload to remove its retained history. Remove only changes metadata for work already proven ended or missing; it does not contact or kill an external session.

### Retry

When a host, SSH connection, or backend cannot be inspected, Tether shows the workload as **Unreachable**. Press `r` to retry observation. Destructive actions stay unavailable until Tether can establish the exact state.

### Acting on a whole group

An orchestration group can be stopped or restarted in one command:

```sh
herdr-tether orchestration stop-workers fleet --dry-run
herdr-tether orchestration stop-workers fleet
herdr-tether orchestration restart-workers fleet
```

A group is a list of workloads to ask about, not an authority of its own.
Membership is metadata, so it can name a workload that has since ended, lost its
ownership proof, or been removed entirely. Every member is therefore resolved
against the records first, and the command prints the plan before it acts: which
workloads it would touch, and which it would leave alone with the reason for
each. `--dry-run` stops after the plan.

Each workload it does act on goes through exactly the same path as
`herdr-tether session stop` and `herdr-tether session restart`, so the private
ownership proof, the exact re-inspections, and the `tmux` guard that checks
identity at execution time all still apply per workload. A group containing a
legacy record with no ownership proof is not a way to act on it: that member is
skipped and says why.

Because it covers more than one workload, it asks before acting, and without a
terminal it refuses rather than assuming consent - pass `--yes` to confirm in a
script. A restart that is paced after an immediate failure stays paced, and the
pace is checked again as each workload's turn comes, so another restart during
the wait cannot slip past it. Only the workloads named in the confirmed plan are
acted on, so editing the group afterwards cannot enlarge what was confirmed.
Each result is printed as that workload finishes rather than in one batch at the
end, so an interrupted run still shows exactly what it had done. One workload
failing does not abandon the rest, and the command exits non-zero if any member
failed or if nothing was acted on after all.

The orchestrator pane is not a worker and is left running; stop it with
`herdr-tether session stop` if you want it gone. A group's orchestrator can be a
worker of a *different* group, and acting on that group does act on it - it is
still a Tether-owned workload, but it is worth knowing before running this
against a group you did not build.

## States shown in the picker

| State | Meaning | Safe actions |
| --- | --- | --- |
| **Running** | The exact owned workload was observed alive. | Open or Stop |
| **Ended** | The owned command exited cleanly, or with an outcome `tmux` could not report. | Restart or Remove |
| **Failed** | The owned command exited with a failing status, which is shown as `[failed]` in the picker and `FAILED` on a Mission Control tile. | Restart or Remove |
| **Failed immediately** | The command exited with a failing status within ten seconds of starting. Restart waits thirty seconds from that end, and the picker says how long is left. | Remove now, Restart when the wait ends |
| **Stopping** | A confirmed Stop is in progress or needs safe reconciliation after interruption. | Wait or retry the visible operation |
| **Unreachable** | Tether cannot currently prove whether the workload is running. | Retry or back out |
| **Removed** | The ended record was finalized and is no longer an active picker item. | None |
| **External** | A discovered non-Tether `tmux` session. | Open only |

Observation and persistence are deliberately separate. A refresh may update what the picker knows, but it cannot silently authorize Stop or Remove. Confirmation and error dialogs stay active until their own operation finishes or you explicitly cancel them.

## Serving is a different question from running

Every state above answers whether the workload's process is alive. Whether the
thing it runs is actually serving is a separate fact, and Tether only knows it
if you configure a `health_command` on the preset the workload was created from
(see [Configuration](configuration.md)). Each status refresh then runs that
command in the workload's directory and shows the verdict beside the state:
`serving`, `not serving` with the exit status, or `health unknown` when the
probe could not run, did not finish, or could not reach the host - Tether never
turns "I could not check" into a verdict either way.

The two axes stay independent on purpose. A live process that is not serving is
a real condition worth seeing, and so is a workload that answers its probe while
Tether cannot confirm the pane is alive. A health result never changes a
workload's state, never unlocks or blocks Stop, Restart, or Remove, and is never
persisted; it is display-only, gathered while a surface is refreshing and
discarded with the view. A workload with no configured probe shows nothing about
serving, because an absent check is not a pass.

## Ended work and history

Tether asks `tmux` for native pane status and exit information when the installed version supports it. This prevents a completed command from lingering as contradictory running/missing work or being offered as resumable.

A failing exit status is kept apart from a clean one, because they are the two outcomes worth telling apart at a glance. Tether records one when it reconciles a workload against `tmux`: when Stop reaps a workload whose command had already exited, or when a restart's verification finds the new incarnation already ended. Nothing watches a workload continuously, so a command that exits while no operation is running is recorded the next time one is. A Mission Control surface that sees a workload in that state asks Herdr for a toast, once per incarnation, which `notifications.workload_failed` controls.

Ended metadata is retained long enough to make Restart and diagnostics useful. Safe finalized history becomes eligible for automatic metadata-only cleanup after the configured retention period (30 days by default). Cleanup never contacts SSH, invokes `tmux`, stops workloads, or expands to newly eligible records during an in-progress operation.

History maintenance is intentionally not part of the normal workflow. Use the advanced CLI maintenance command only when you need to inspect or clear eligible history explicitly; run `herdr-tether session --help` for the commands supported by the installed version.

## Ownership boundary

Tether reserves generated `tether-*` identities for workloads it creates. Lifecycle actions use exact recorded host and session identities.

Existing non-Tether sessions may appear as **External**. Tether can validate and attach to their exact names, but it does not adopt, persist, rename, stop, remove, or otherwise manage them. A name in Tether's reserved namespace is never downgraded to an external session merely because its metadata is unavailable.

Records migrated from Tether 0.2.x do not contain the private ownership proof required by 0.3. Tether labels them **Legacy**, never offers Open, Stop, or Restart, and does not adopt or inspect a same-named `tmux` session. Use `x` in the picker or `herdr-tether session remove <id>` to remove only the legacy metadata; any workload with that name is left untouched.

## Failure behavior

- **The view closes or SSH drops:** the workload keeps running.
- **Open fails:** Tether leaves the workload untouched and keeps a visible retry/cancel error.
- **Create cannot be recorded safely:** Tether attempts exact rollback and reports if the created identity may still need inspection.
- **Stop is interrupted:** recoverable state records that Stop began; the workload is not shown as safely running or ended without reconciliation.
- **Restart placement fails:** the invoking pane is preserved and the operation remains recoverable.
- **State is corrupt or from a future schema:** Tether fails closed rather than guessing.

For recovery steps, see [Troubleshooting](troubleshooting.md). For implementation and trust boundaries, see [Architecture and security](architecture.md).
