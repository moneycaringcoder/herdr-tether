# Roadmap

Ideas for future work, roughly in the order they would most improve Tether. None
of this is committed to a release, and nothing here is a promise.

Two boundaries are settled and nothing here reopens them: **Tether does not
provide remote Herdr federation or pane streaming**, and **only Tether-owned
workloads can be stopped or removed**. Discovered external `tmux` sessions stay
attach-only.

## Correctness

### Finish the `EINTR` audit

v0.7.2 fixed `read_bounded_line`, which called `fill_buf()?` with no
`ErrorKind::Interrupted` retry, so a signal arriving during a blocking socket read
surfaced as `Read rejected: Interrupted system call (os error 4)`. The same fix
also repaired the Mission Control event-subscription loop, which had been tearing
down and reconnecting on every interruption.

Two callers were found by following one failure. The sweep was not exhaustive.
Every blocking read and write in the socket, `tmux`, and SSH paths should be
audited for the same shape, and the pattern documented so it is not reintroduced.

### Publish a compatibility matrix per release

`doctor` already reports a protocol above the verified ceiling as "newer and not
yet verified", which is the right behaviour at runtime. What a user cannot see
before installing is which Herdr versions a given Tether release was actually
exercised against. The upstream canary produces exactly that evidence on a
schedule; publishing it turns a private signal into something a user can check.

### Cover the picker's worktree paths

The picker brings the invoking pane's sibling Git worktrees forward.
`--separate-git-dir` and submodule layouts are the cases most likely to make that
resolution wrong, and they are not covered.

## Lifecycle

### Crash-loop detection and restart backoff

A restart that immediately fails is currently a restart that immediately fails
again if the user presses `Enter` again. Detecting the loop and backing off turns
a frustrating cycle into a legible one, without ever restarting anything on the
user's behalf.

### Health checks

`Ended` is derived from the process. "The process is alive" and "the thing is
actually serving" are different facts, and for a dev server or a watcher the
second is the one that matters. An optional per-workload health command would
report it, with an explicit `unknown` when it cannot run — never an inferred pass.

### Notify on non-zero exit

A workload that ends with a failing status is exactly the event worth surfacing,
and it is currently indistinguishable at a glance from one that finished cleanly.

## Mission Control

### Group-level operations

The orchestration group already organizes workloads and holds explicit
permissions. Acting on the group — restart every worker, stop every worker —
is the obvious next step, and it must go through the same explicit confirmation
and ownership checks as the single-workload path, not around them.

### A sharper `UNREACHABLE` versus `STALE` distinction

Both mean "the tile is not telling you the truth right now", for different
reasons and with different remedies. The tiles name them; the difference could be
explained where the user is looking rather than in the documentation.

### A bounded output preview per tile

Enough recent output to recognize what a workload is doing, without inferring
agent state from it. State comes from Herdr's event stream and typed socket API
for recognized agents, and from bounded `tmux` capture otherwise — a preview must
not become a third, guessed source of truth.

## Operating it

### A structured audit trail

Lifecycle transitions are the thing people reconstruct after something goes wrong.
A structured, bounded record of them — never including prompt text, terminal
contents, or credentials, exactly as state and logs already exclude them — would
make that reconstruction possible.

### Per-workload resource reporting

Where the backend can supply it. Answers "which of these twenty is eating the
machine", which is otherwise a manual hunt.

### Preset sharing

Presets are trusted code: Tether starts a login shell and runs the command
through it, and does not sandbox anything. Any sharing mechanism has to lead with
that, show the exact command before it is adopted, and never fetch a preset
implicitly.
