---
name: tether
version: 1.0.0
description: Observe and operate durable local or remote development workloads through Tether's public CLI without depending on tmux internals.
author: Tether maintainers
license: MIT
platforms: [macos, linux]
metadata:
  hermes:
    tags: [Development, Orchestration, Remote Workloads, Tether]
---

# Tether

Use Tether as the visibility and lifecycle boundary for durable development workloads. Hermes is only a CLI client: do not import, embed, or require Hermes in Tether, and do not bypass Tether with direct tmux commands.

## Install

Choose the source that matches the Tether installation:

**Stable v0.3.0:**

```sh
hermes skills install https://raw.githubusercontent.com/moneycaringcoder/herdr-tether/v0.3.0/integrations/hermes/SKILL.md
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
- start a user-chosen harness or command in a Tether-owned workload;
- reopen, restart, stop, or remove a specific Tether-owned workload; or
- compare workload state across configured local and remote hosts.

Do not use it to build a background observer, orchestration groups, automatic remediation, or a replacement scheduler.

## Preflight

1. Resolve the Tether executable from the current environment. Use `herdr-tether` unless the user supplied another executable name. Do not guess an absolute path.
2. Run `herdr-tether --version` and `herdr-tether snapshot`. If either command is unavailable, explain that Tether must be installed and configured on the machine where Hermes executes.
3. Parse snapshot stdout as JSON. Require `schema_version` to equal `1`; if it does not, stop and report the unsupported version rather than guessing field meanings.
4. Treat `completion: "partial"` as usable degraded data, not as an empty or complete inventory. Preserve each typed status when reporting why a host, repository scan, workload probe, or external catalog is incomplete.

`snapshot` is the sole machine-readable observation API for this skill. It is bounded, read-only, and safe to repeat on demand. Never scrape the interactive picker or terminal canvas.

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
3. **Workload command:** use exactly one of a named Tether preset or an explicit command supplied/approved by the user. A harness (for example an agent CLI, test runner, shell, or editor) is just that configured command; this skill does not require a particular harness.
4. **Environment:** rely on the selected host's configured shell environment, environment manager, or secret manager. Never place credentials or tokens in CLI arguments or persisted command text.
5. **Layout:** add a placement only when invoked from a compatible Herdr pane and the user chose one: `split-right`, `split-down`, `new-tab`, or `replace-current-pane`. Otherwise omit `--placement` and let Tether configuration decide.

Preset form:

```sh
herdr-tether open --host HOST --directory DIRECTORY --preset PRESET
```

Explicit-command form:

```sh
herdr-tether open --host HOST --directory DIRECTORY --command COMMAND
```

Add `--placement PLACEMENT` only under the layout rule above. Pass every substituted value as one safely quoted argument; never construct a command by concatenating untrusted text. `open` intentionally enters the durable terminal, so run it only from an interactive surface that can hand control to Tether. Do not simulate a detached launch with raw tmux.

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

## Safety Boundaries

- Never read or edit Tether configuration, state, audit, or lock files directly.
- Never call tmux or SSH to discover or mutate workloads behind Tether's ownership checks.
- Never use hidden/internal Tether commands or infer backend session names.
- Never expose credentials, environment values, raw errors, or private paths in summaries.
- Never turn repeated snapshots into a daemon, watcher, scheduled observer, or autonomous remediation loop.
- Never stop, restart, remove, or attach to an external catalog entry.
- When data is ambiguous, ask the user to choose; do not select a host, directory, command, preset, placement, or destructive action by convenience.

## Verification

A successful observation has parseable JSON with `schema_version: 1`, an explicit top-level `completion`, and typed per-host statuses. A successful launch or lifecycle request is verified by a subsequent snapshot showing the exact owned workload and expected state. Report partial collection and typed degradation verbatim; never replace uncertainty with a success claim.
