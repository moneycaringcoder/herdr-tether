# Roadmap

What is settled, what has been decided against, and where open work is tracked.

An entry in this file is work that has not been done. An idea is removed here in
the change that ships it, so a reader never has to guess whether a paragraph
describes a plan or a property the code already has. Open work lives in the issue
tracker rather than being restated here, because an issue has a state and a
paragraph does not.

## Settled boundaries

Nothing here reopens these:

- **Tether does not provide remote Herdr federation or pane streaming.** Remote
  support is OpenSSH and `tmux` on the selected machine.
- **Only Tether-owned workloads can be stopped or removed.** Discovered external
  `tmux` sessions stay attach-only, and a group operation does not become a way
  around that.

The full trust boundary, including what Tether treats as trusted code and what it
never persists, is in [Architecture and security](architecture.md) and
`SECURITY.md`.

## Decided against

### Preset sharing

Presets already share: sending the `[[hosts.presets]]` block is the mechanism, and
[Configuration](configuration.md) documents it, including which host a pasted block
binds to and what a rejected one costs.

An import command was considered and declined on cost rather than on safety. One
that printed the exact `command` and `health_command`, named the host and directory
they would bind to, and required typed confirmation would show a recipient what
reading the block shows them. It would also be a permanent surface, a file format,
and a second way to add a preset, in exchange for a paste — and it would make
preset text something Tether itself takes from elsewhere and then executes, which
is the one intake boundary worth not moving for convenience.

What remains worth doing is unrelated to sharing and is tracked as issue #110:
showing a preset's command before it runs, for every preset rather than only for
adopted ones.

### Naming the workload in a toast

A toast carries a short workload reference rather than the tile's display title,
and [Configuration](configuration.md) documents why. Letting the title travel
would read better with several workloads open, and it was declined because the
title is generated from host, repository name, and preset, while a toast is
rendered by Herdr and can land on a surface Tether does not control.

Distinguishing a title a person typed from one Tether generated, and letting only
the first travel, would satisfy both. It was declined on cost: it needs provenance
on the orchestration member, which is a state schema change and a second class of
member, for text the tile beside the toast already shows.

## Shipped

Everything this file previously listed as an idea has been built, and the entries
have been retired rather than left to read as plans:

- Finishing the `EINTR` audit across the socket, `tmux`, and SSH paths.
- Worktree resolution for `--separate-git-dir` and submodule layouts.
- A published Herdr compatibility matrix per release, `compatibility.md`.
- Naming a workload that ended with a failing status, and a toast for it.
- Pacing the restart of a workload that failed immediately, without ever
  restarting anything on the user's behalf.
- Optional per-workload health commands, with an explicit `unknown` rather than an
  inferred pass.
- Telling `UNREACHABLE` and `STALE` apart in the interface.
- Group-level stop and restart, through the same ownership checks and confirmation
  as the single-workload path.
- A bounded output preview per Mission Control tile that never becomes a second
  source of agent state.
- Per-workload resource reporting, with absence stated rather than drawn as zero.
- A structured, bounded audit trail of lifecycle transitions.

Each shipped with the constraint its entry named, and with tests defending it. The
release notes in `CHANGELOG.md` say what each one does.
