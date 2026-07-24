# Security Policy

## Supported versions

Security fixes are provided for the latest version published through the project's [GitHub releases](https://github.com/moneycaringcoder/herdr-tether/releases). Development snapshots and older versions are not supported security branches.

Reproduce a suspected issue on the latest published version when it is safe to do so. A report affecting an older version is still useful when the same behavior may remain current.

## Reporting a vulnerability

Report suspected vulnerabilities through [GitHub private vulnerability reporting](https://github.com/moneycaringcoder/herdr-tether/security/advisories/new). Do not disclose exploit details, credentials, hostnames, repository paths, terminal output, or other sensitive data in a public issue.

A useful report includes:

- the affected Tether, Herdr, operating-system, OpenSSH, and `tmux` versions;
- impact and the security boundary crossed;
- minimal reproduction steps using disposable data;
- whether exploitation requires trusted configuration, local account access, or control of a remote host; and
- any known mitigation.

Maintainers aim to acknowledge a complete report within seven days and provide an initial assessment or request for more information within fourteen days. These are response goals, not guarantees. Remediation and disclosure timing depend on severity and complexity. Maintainers will coordinate an advisory, credit, and disclosure date when appropriate; urgent user protection may require earlier disclosure.

## Security boundary

Tether manages local or SSH-backed `tmux` workloads and integrates with a local Herdr installation. Remote transport is ordinary OpenSSH, not Herdr federation or remote pane streaming.

The trusted computing base includes:

- the local user account and filesystem;
- installed Tether, Herdr, `ssh`, `tmux`, `/bin/sh`, Git, Rust, and Cargo executables;
- user-controlled Tether configuration and command presets;
- OpenSSH configuration, keys, agent, proxies, and `known_hosts`; and
- every selected remote host and its user account.

Tether does not sandbox configured commands, secure a compromised endpoint, configure SSH trust, rotate credentials, or replace host authorization. Presets are trusted code. Protect the user account, configuration, state, and backups accordingly.

Within that boundary, Tether:

- invokes OpenSSH with validated targets, `BatchMode=yes`, bounded connection
  setup, and user-private `%C` control sockets while preserving host-key policy;
- preserves separated argv and POSIX-quotes values only where a remote shell or Herdr pane command requires a command line;
- uses exact `tmux` targets for owned lifecycle actions;
- reserves `tether-*` names and permits only validated nonreserved external names through an attach-only path;
- never adopts, persists, renames, stops, removes, or otherwise manages discovered external sessions;
- treats unreachable or unknown observation as unsafe for destructive action;
- bounds status and repository discovery with deadlines, output caps, cancellation, and child-process cleanup;
- bounds SSH alias `Include` discovery by canonical directory, cycle, depth,
  file-count, and byte limits;
- records lifecycle transitions so interrupted Stop and Restart operations remain recoverable;
- keeps metadata cleanup separate from SSH, `tmux`, probing, and Stop capabilities;
- migrates and writes configuration, lifecycle/orchestration state, and Agent view preferences with advisory locks, private Unix permissions, and atomic replacement;
- excludes command bodies, child output, raw backend errors, and private storage paths from scriptable snapshots;
- treats explicit agent hints, Agent view filters, and pane metadata tokens as
  presentation/binding evidence that cannot authorize capture, input, or
  lifecycle mutation by themselves;
- delivers a Mission Control prompt only after explicit capability, exact
  membership epoch, ownership, running-state, recognized-agent, settled-state,
  and reviewed-destination checks; prompt text is memory-only and uncertain
  delivery is never retried automatically; and
- stores no SSH passwords, private keys, access tokens, terminal contents, prompt
  text, or telemetry identifiers.

Detailed component, lifecycle, persistence, and trust boundaries are documented in [Architecture and security](docs/architecture.md).
