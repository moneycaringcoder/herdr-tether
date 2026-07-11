# Security Policy

## Supported versions

Security fixes are provided for the latest released version.

| Version | Supported |
| --- | --- |
| 0.2.x | Yes |
| 0.1.x and development snapshots | No |

Users should reproduce a suspected issue on the latest release when it is safe to do so. A report affecting an older version is still useful when the same behavior may remain current.

## Reporting a vulnerability

Report suspected vulnerabilities through [GitHub private vulnerability reporting](https://github.com/moneycaringcoder/herdr-tether/security/advisories/new). Do not disclose exploit details, credentials, hostnames, repository paths, terminal output, or other sensitive data in a public issue.

A useful report includes:

- the affected Tether, Herdr, operating-system, OpenSSH, and `tmux` versions;
- impact and the security boundary crossed;
- minimal reproduction steps using disposable data;
- whether exploitation requires trusted configuration, local account access, or control of a remote host;
- any known mitigation.

Maintainers aim to acknowledge a complete report within seven days and provide an initial assessment or request for more information within fourteen days. These are response goals, not guarantees. Remediation and release timing depend on severity, complexity, and maintainer availability. Please allow time for a fix and supported-version release before public disclosure. Maintainers will coordinate an advisory, credit, and a disclosure date with the reporter when appropriate; urgent user protection may require earlier disclosure.

## Threat and trust boundary

Tether manages local or SSH-backed `tmux` workloads and integrates with a local Herdr 0.7.3-or-newer installation. Its remote transport is ordinary OpenSSH, not Herdr federation or remote pane streaming.

The trusted computing base includes:

- the local user account and filesystem;
- the installed Tether, Herdr, `ssh`, `tmux`, `/bin/sh`, Git, Rust, and Cargo executables;
- user-controlled Tether configuration and command presets;
- OpenSSH configuration, keys, agent, proxies, and `known_hosts` policy;
- every selected remote host and its user account.

Tether does not sandbox configured commands, secure a compromised local or remote endpoint, configure SSH trust, rotate credentials, or replace host authorization. Presets are trusted code executed by `/bin/sh -lc`. Configuration may contain command bodies and targets; session state contains host targets, directories, preset labels, identifiers, lifecycle status, and timestamps. Protect the user account, configuration, state, and backups accordingly.

Within that boundary, Tether:

- invokes the system OpenSSH client with validated targets and `BatchMode=yes` while preserving the user's strict host-key policy;
- preserves separated argv and POSIX-quotes values only where a remote shell or Herdr pane command requires a command line;
- uses exact `tmux` targets for owned lifecycle actions;
- reserves `tether-*` names for owned work and permits only validated nonreserved external names through a non-owning attach path;
- never adopts, persists, renames, closes, kills, marks, or prunes discovered external sessions;
- performs bounded status and repository-discovery work with deadlines, output caps, cancellation, and child-process cleanup;
- records lifecycle transitions so an interrupted close remains recoverable as Closing rather than appearing Active or Closed;
- gives prune no SSH, `tmux`, probe, or close capability and revalidates the exact confirmed Closed records before removal;
- migrates and writes configuration/state with advisory locks, private Unix permissions, and atomic replacement;
- excludes configured command bodies, child output, raw backend errors, and storage paths from the scriptable snapshot;
- stores no SSH passwords, private keys, access tokens, terminal contents, or telemetry.

The detailed component and lifecycle boundaries are documented in [Tether architecture](docs/architecture.md).
