---
name: tether
version: 1.1.0
description: Observe and operate durable workloads and opt-in groups through Tether's public CLI without depending on tmux internals.
author: Tether maintainers
license: MIT
platforms: [macos, linux]
metadata:
  hermes:
    tags: [Development, Orchestration, Remote Workloads, Tether]
---

# Tether

Use Tether as the visibility and lifecycle boundary for durable development workloads. Hermes is a configurable reference adapter only: do not import, embed, or require Hermes in Tether, and do not bypass Tether with direct tmux commands.

## Install

Choose the source that matches the Tether installation:

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

The skill is available in new Hermes sessions after installation.

## When to Use

Use this skill when a user wants to:

- see repositories or durable workloads visible to Tether;
- start a user-chosen command in a Tether-owned workload;
- reopen, restart, stop, or remove a specific Tether-owned workload;
- compare workload state across configured local and remote hosts; or
- configure and view an opt-in orchestration group on stable v0.4.0,
  development `main`, or an immutable reviewed commit.

## Preflight

1. Resolve the Tether executable from the current environment. Use `herdr-tether` unless the user supplied another executable name. Do not guess an absolute path.
2. Run `herdr-tether --version` and `herdr-tether snapshot`. If either command is unavailable, explain that Tether must be installed and configured on the machine where Hermes executes.
3. Parse snapshot stdout as JSON. Require `schema_version` to equal `1`; if it does not, stop and report the unsupported version rather than guessing field meanings.
4. Treat `completion: "partial"` as usable degraded data, not as an empty or complete inventory. Preserve each typed status when reporting why a host, repository scan, workload probe, or external catalog is incomplete.

`snapshot` is the sole machine-readable workload-observation API for this skill. It is bounded, read-only, and safe to repeat on demand. `orchestration list --json` reports group metadata only. Never scrape the interactive picker or terminal canvas.

## Read a Snapshot

Run:

```sh
herdr-tether snapshot
```

Use `--pretty` only when showing the JSON to a person. For each host, report:

- `name`, `origin`, and typed `reachability`/`discovery` status;
- discovered `repositories` and any `root_errors`;
- `owned_sessions`, including metadata/workload status and attachment count; and
- the safe `external_catalog` summary.

Keep these distinctions:

- `owned_sessions` are Tether lifecycle records and are the only sessions eligible for Tether lifecycle commands.
- `external_catalog.sessions` are visible but unowned. Never stop, remove, restart, or otherwise mutate them.
- A host with `origin: "state"` preserves historical owned metadata but is not necessarily a currently configured launch target.
- `not_collected`, `unreachable`, timeout, limit, and error statuses are unknown/degraded results, never proof that no workload exists.

Do not seek excluded data. Snapshot intentionally omits command bodies, ownership proofs, backend identifiers, terminal output, credentials, raw backend errors, and Tether's private storage paths.

## Start a Visible Workload

Only start a workload after the user has requested it and the launch choices are known. Resolve each choice independently instead of assuming a particular host, harness, repository layout, shell, or environment:

1. **Host:** choose `local` or a currently effective configured/SSH host visible in the snapshot. Do not launch against a historical `state`-only target.
2. **Directory:** use the user-selected repository/directory appropriate to that host. Paths are host-relative; never transplant a local machine path to a remote host.
3. **Workload command:** use exactly one of a named Tether preset or an explicit command supplied/approved by the user. The command and its runtime are external configuration; this skill does not require a particular agent, scheduler, or tool.
4. **Agent identity:** pass `--herdr-agent KIND` only when the user explicitly supplied a Herdr-supported kind for an agent hidden behind `tmux` or SSH. Never infer it from the command, repository, or process name.
5. **Environment:** rely on the selected host's configured shell environment, environment manager, or secret manager. Never place credentials or tokens in CLI arguments or persisted command text.
6. **Layout:** add a placement only when invoked from a compatible Herdr pane and the user chose one: `split-right`, `split-down`, `new-tab`, or `replace-current-pane`. Otherwise omit `--placement` and let Tether configuration decide.

Preset form:

```sh
herdr-tether open --host HOST --directory DIRECTORY --preset PRESET
```

Explicit-command form:

```sh
herdr-tether open --host HOST --directory DIRECTORY --command COMMAND
```

Add `--herdr-agent KIND` and `--placement PLACEMENT` only under the rules above. Pass every substituted value as one safely quoted argument; never construct a command by concatenating untrusted text. `open` intentionally enters the durable terminal, so run it only from an interactive surface that can hand control to Tether. Do not simulate a detached launch with raw tmux.

After control returns, request a fresh snapshot and identify the resulting owned workload from observable fields. Do not claim creation succeeded merely because the attach command started.

## Operate an Owned Workload

Select an exact ID from `hosts[].owned_sessions`; never invent an ID and never take one from `external_catalog.sessions`.

```sh
herdr-tether session open ID
herdr-tether session restart ID
herdr-tether session stop ID
herdr-tether session remove ID
```

- `open` is for an existing running owned workload.
- `restart` is for an ended owned workload and enters its durable terminal.
- `stop` and `remove` mutate lifecycle state. Run either only on an explicit user request, state the selected host and ID first, and do not broaden the action to other workloads.
- `remove` is not a substitute for stopping a running workload.

Read a new snapshot after a requested lifecycle action and verify the expected observable metadata/workload status. If the result is partial, report the action result separately from any status that could not be collected.

## Configure an Orchestration Group

First perform the side-effect-free capability probe:

```sh
herdr-tether orchestration --help
```

Require a successful exit and help that lists the orchestration subcommands.
If the probe fails or the command is absent, stop and report that this Tether
installation does not provide orchestration; do not attempt any orchestration
mutation.

The user or calling adapter must provide all orchestration values externally:

- a safe group ID and display title;
- one exact orchestrator session ID;
- each exact worker session ID and optional title;
- `observe_output`, `open_interactive`, or both for every worker; and
- any host selection, workload command, directory, and Herdr placement.

Do not infer these values from a repository name, machine layout, process title,
or assumed agent topology. Resolve session IDs from Tether's public snapshot,
never from external catalog entries or tmux. Create only after the user has
approved the complete mapping:

```sh
herdr-tether orchestration create GROUP --title TITLE --orchestrator SESSION
herdr-tether orchestration add-worker GROUP SESSION --title TITLE \
  --observe-output --open-interactive
herdr-tether orchestration list --json
```

Repeat `add-worker` once per approved worker, passing only its approved
capabilities. At least one capability is required. `observe_output` authorizes
bounded read-only capture of a running exact-owned worker;
`open_interactive` independently authorizes `Enter` to open that worker when it
is still running and exact-owned. Neither capability authorizes lifecycle
mutation or worker input.

To remove only membership or group metadata:

```sh
herdr-tether orchestration remove-worker GROUP SESSION
herdr-tether orchestration delete GROUP
```

These commands never stop or remove referenced sessions. Re-run
`orchestration list --json` to verify the exact resulting membership.

## Launch Observer from Herdr

Observer is an interactive companion view and must be launched from a Herdr
pane:

```sh
herdr-tether orchestration observe GROUP --placement split-right
```

The command creates one outer Herdr pane. That pane renders up to four
read-only worker tiles per deterministic page and refreshes membership and
lifecycle labels while it runs. It never forwards input to a worker. `Enter`
opens only a capability-authorized, running, exact-owned worker as an ordinary
Tether view while Observer remains.

A Hermes process running inside a Tether `tmux` workload must not assume it has
Herdr's plugin pane environment. In that situation, request or hand off the
explicit command and approved values to a Herdr-pane context instead of
attempting to reconstruct environment variables. Prefer `split-right`,
`split-down`, or `new-tab`; Tether normalizes `replace-current-pane` to
`split-right` so the invoking or Observer pane remains available.

The native **Show group in Agents sidebar** and **Restore default Agents
sidebar** actions are user-facing Herdr plugin surfaces, not public
orchestration CLI commands. An adapter must not call Herdr's socket directly,
edit `agent-view.json`, or claim that creating a group activated a sidebar
filter.

## Safety Boundaries

- Never read or edit Tether configuration, state, audit, or lock files directly.
- Never call tmux or SSH to discover or mutate workloads behind Tether's ownership checks.
- Never use hidden/internal Tether commands or infer backend session names.
- Never expose credentials, environment values, raw errors, or private paths in summaries.
- Never turn repeated snapshots into a daemon, watcher, scheduled observer, or autonomous remediation loop.
- Never stop, restart, remove, or attach to an external catalog entry.
- When data is ambiguous, ask the user to choose; do not select a host, directory, command, preset, placement, or destructive action by convenience.

## Verification

A successful workload observation has parseable snapshot JSON with `schema_version: 1`, an explicit top-level `completion`, and typed per-host statuses. A successful launch or lifecycle request is verified by a subsequent snapshot showing the exact owned workload and expected state. A successful group mutation is verified separately with `orchestration list --json`; it is not proof that a referenced workload is running. Report partial collection and typed degradation verbatim; never replace uncertainty with a success claim.
