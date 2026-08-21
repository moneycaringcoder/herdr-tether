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

### Remove

Press `x` on an ended workload to remove its retained history. Remove only changes metadata for work already proven ended or missing; it does not contact or kill an external session.

### Retry

When a host, SSH connection, or backend cannot be inspected, Tether shows the workload as **Unreachable**. Press `r` to retry observation. Destructive actions stay unavailable until Tether can establish the exact state.

## States shown in the picker

| State | Meaning | Safe actions |
| --- | --- | --- |
| **Running** | The exact owned workload was observed alive. | Open or Stop |
| **Ended** | The owned command exited cleanly, or with an outcome `tmux` could not report. | Restart or Remove |
| **Failed** | The owned command exited with a failing status, which is shown as `[failed]` in the picker and `FAILED` on a Mission Control tile. | Restart or Remove |
| **Stopping** | A confirmed Stop is in progress or needs safe reconciliation after interruption. | Wait or retry the visible operation |
| **Unreachable** | Tether cannot currently prove whether the workload is running. | Retry or back out |
| **Removed** | The ended record was finalized and is no longer an active picker item. | None |
| **External** | A discovered non-Tether `tmux` session. | Open only |

Observation and persistence are deliberately separate. A refresh may update what the picker knows, but it cannot silently authorize Stop or Remove. Confirmation and error dialogs stay active until their own operation finishes or you explicitly cancel them.

## Ended work and history

Tether asks `tmux` for native pane status and exit information when the installed version supports it. This prevents a completed command from lingering as contradictory running/missing work or being offered as resumable.

A failing exit status is kept apart from a clean one, because they are the two outcomes worth telling apart at a glance. When a Mission Control surface is open and observes a workload reaching a failing end, it also asks Herdr for a toast naming the workload and its exit status, which `notifications.workload_failed` controls. Nothing observes a workload while no Tether surface is running, so the notice arrives when the end is observed rather than the moment it happens.

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
