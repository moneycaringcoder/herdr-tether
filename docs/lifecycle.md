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

Repeats count. A workload that fails immediately, is restarted, and fails
immediately again is in a loop rather than having bad luck, so the wait doubles
each time: thirty seconds, then a minute, then two, then four, stopping at five
minutes however many failures follow. Every surface that mentions the wait says
how many failures in a row are behind it, because a wait that grows without
saying why reads as Tether making the workload harder to restart.

The count is kept on the record as timestamps, so it survives the restart it
exists to describe. Starting again does not end the run: a workload in a loop
starts successfully every time, so treating a start as recovery would hold the
count at one and the wait would never grow. What ends a run is an end of a
different shape - a clean exit, or a failure that arrives after the workload had
been running for a while - or simply time, since only failures from the last hour
are counted. A workload that fails immediately today does not inherit a wait from
failures last week, and the history holds at most sixteen entries. Nothing about
the pace changes what Tether does: it never restarts anything itself, so a longer
wait is a longer refusal to offer the action, with the reason attached.

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
| **Failed immediately** | The command exited with a failing status within ten seconds of starting. Restart waits thirty seconds from that end, doubling for each repeat up to five minutes, and the picker says how long is left and how many failures in a row. | Remove now, Restart when the wait ends |
| **Stopping** | A confirmed Stop is in progress or needs safe reconciliation after interruption. | Wait or retry the visible operation |
| **Unreachable** | Tether cannot currently prove whether the workload is running. | Retry or back out |
| **Removed** | The ended record was finalized and is no longer an active picker item. | None |
| **External** | A discovered non-Tether `tmux` session. | Open only |

Observation and persistence are deliberately separate. A refresh may update what the picker knows, but it cannot silently authorize Stop or Remove. Confirmation and error dialogs stay active until their own operation finishes or you explicitly cancel them.

Tether separates an answer from a failure to ask. An answer is a `tmux` that ran
and reported on the socket it uses: either the session with that name and
ownership proof, or no such session, or no server running there at all. All three
are evidence, and the last two end the record - a host whose last session ended
has no server, because `tmux` exits with its final session, so treating that as
unproven would leave every record on a rebooted machine impossible to stop,
restart, or remove.

A failure to ask is not evidence, and Stop, Restart, Remove, and group actions all
refuse it and change nothing: a `tmux` that is not installed, one that cannot be
executed, one that rejected the query, or one whose output could not be trusted.
A `tmux` too old for the inspection Tether needs falls here, and reads as a
workload whose state cannot be proven rather than as one that ended.

The limit worth knowing: `tmux` chooses its socket from the environment, so what
Tether can see is the server its own invocation reaches. A workload started under
a different `TMUX_TMPDIR` is not visible to a Tether run without it, and reads as
ended. Recording the socket a workload was created on would close that gap.

## Noticing an end nobody asked about

A workload that finishes on its own is noticed by the next ordinary refresh,
without anyone acting on it. `remain-on-exit` keeps a finished workload's session
listed, so presence alone proves nothing; the same `list-sessions` a refresh
already runs now also reports the session's active pane - whether it is dead, and
the status or signal it ended with. A row that was `[running]` becomes `[ended]`,
or `[failed]` when the command exited with a failing status or was killed by a
signal, and its action becomes Restart. The record is reconciled at the same time,
so the action the row offers is one the record accepts.

The cost is nothing extra: one `list-sessions` per host, as before, with more
fields in the same format string. A host with twenty workloads costs the same as a
host with one, and no per-workload inspection is added to any refresh.

There is a limit, and it comes from what `tmux` can report about a session. The
pane fields describe the session's *active* pane, and Tether launches exactly one
window holding one pane - so while a workload still has that shape, its active
pane is its work. Once someone attaches and splits it, or opens a second window,
a dead active pane says nothing about the workload's own command: a split that
exited would otherwise read as the workload ending. Tether says nothing in that
case rather than guessing, so a workload that has been split keeps reading as
running until an operation inspects it exactly.

In the `snapshot` JSON such a workload reads `workload_status: "exited"`.

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

## What a workload is using

A running row also carries what that workload's processes are using on their
host, so the answer to "which of these twenty is eating the machine" is on the
screen rather than the result of a manual hunt:

```
[running] [running] Tether · Open …0000002 · /srv/app · shell · 144% · 1.5G
```

The figure comes after the row's own words, not in front of them: rows truncate
from the right, and knowing which workload a number belongs to matters more than
the number.

A workload is a `tmux` session rather than a single process, and its pane's shell
is usually idle while a child does the work, so these are the totals for every
process under the workload's panes. Processor share is what the host reports, so
it can exceed 100% on a multi-core host - which is exactly the case worth seeing.

Processor share is what the workload used while Tether watched, not an average
over its life. `ps` reports the latter, which would round a workload that has been
up for days and just started eating a core down to nothing, so Tether takes two
samples of cumulative processor time a second apart and reports the difference.

Tether asks each host two questions per refresh, whatever the number of workloads
on it: which process each pane belongs to, and what its processes are using. A
host with twenty workloads therefore costs the same as a host with one, the whole
phase is bounded so a slow host delays figures rather than the refresh, and a host
with nothing running is not asked at all.

Resident memory is summed per process, so pages shared between a workload's own
processes are counted once for each of them. A workload that forks eight workers
over one heap therefore reads high; the figure is a comparison between workloads
rather than an accounting of the machine.

Absence is stated rather than drawn as a zero. `usage unknown` on a row means the
host could not be asked, did not answer, answered unusably, or does not account
for that workload's processes - a host that cannot report is not a workload using
nothing, and a zero would say the opposite. An unreachable host is not asked at
all: the liveness check already proved it cannot answer, so its workloads read as
unknown once rather than the refresh relearning the same failure. Stopped
workloads carry no figure, because a workload that is not running is not using
anything.

Like health, these figures are display-only. They are never written to
`state.json`, never change what actions are available, and are not part of the
`snapshot` JSON, which reports what Tether observed about a workload's lifecycle
rather than what its host is spending on it.

## Ended work and history

Tether asks `tmux` for native pane status and exit information when the installed version supports it. This prevents a completed command from lingering as contradictory running/missing work or being offered as resumable.

A failing exit status is kept apart from a clean one, because they are the two outcomes worth telling apart at a glance. Tether records one when it reconciles a workload against `tmux`: when Stop reaps a workload whose command had already exited, or when a restart's verification finds the new incarnation already ended. Nothing watches a workload continuously, so a command that exits while no operation is running is recorded the next time one is. A Mission Control surface that sees a workload in that state asks Herdr for a toast, once per incarnation, which `notifications.workload_failed` controls.

Ended metadata is retained long enough to make Restart and diagnostics useful. Safe finalized history becomes eligible for automatic metadata-only cleanup after the configured retention period (30 days by default). Cleanup never contacts SSH, invokes `tmux`, stops workloads, or expands to newly eligible records during an in-progress operation.

History maintenance is intentionally not part of the normal workflow. Use the advanced CLI maintenance command only when you need to inspect or clear eligible history explicitly; run `herdr-tether session --help` for the commands supported by the installed version.

## Reconstructing what happened

A record says where a workload ended up. What people actually want after
something goes wrong is the order things happened in, so Tether keeps a trail of
the transitions themselves: the reservation, the activation, a Stop beginning and
finishing, a restart's new incarnation, a reconciliation that found the work
already ended, and a removal. Each line carries when it happened, which workload,
which `tmux` incarnation, and the exit status when one was observed. Run
`herdr-tether session history` to read it.

The trail says nothing about where a workload runs or what it does. No host, no
target, no directory, no command, no preset, no ownership proof - the same
exclusions state and notifications already enforce, and more of them, because a
trail is the thing someone pastes into a bug report.

A transition that changed nothing is not recorded. The picker reconciles every
ended workload against `tmux` on every refresh, and each of those confirms what
the record already said; a line for each would bury the creates, stops, and
restarts rather than adding to them.

It is bounded twice over. Transitions are dropped once they pass the same
`retention.closed_days` window that decides how long closed workloads are kept,
and a ceiling on the number of entries stops one busy day from growing the file
without limit; the oldest go first. The window applies to what a read shows as
well as to what a write keeps, so a quiet installation - the one recording
nothing new - does not go on showing history the window says is gone. The trail
is a sibling of `state.json` with its own schema, so reading current state never
carries the whole history.

It is a record of work, never a precondition for it. A transition that cannot be
written to the trail is dropped rather than failing the operation that already
succeeded, so a missing line means bookkeeping failed, not that the work did. A
trail that cannot be read at all is moved aside and a fresh one started, because
the alternative is an installation that goes on reporting successful stops and
restarts while silently keeping no record of any of them.

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
