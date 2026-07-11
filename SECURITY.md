# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities through [GitHub private vulnerability reporting](https://github.com/moneycaringcoder/herdr-tether/security/advisories/new). Do not include exploit details, credentials, hostnames, or other sensitive data in a public issue. Include the affected version, platform, reproduction steps, impact, and any suggested mitigation. Maintainers will acknowledge the report and coordinate disclosure and remediation through the private advisory.

## Security model

Tether 0.1 supports macOS and Linux and manages durable `tmux` sessions locally or over ordinary OpenSSH connections. Remote operation is not native Herdr federation or remote pane streaming. Herdr integration opens a local plugin overlay and places resume commands in a split or tab.

Tether:

- invokes the system `ssh` client directly, with the remote target as a separate argument, and relies on OpenSSH authentication and the user's `known_hosts` and strict host-key policy;
- performs one explorer tmux catalog probe per host non-interactively with null stdin, a three-second deadline, fixed concurrency, 64 KiB output caps, cancellation, and process-group cleanup; the validated catalog derives both owned status and attachable external rows, while remote probes preserve OpenSSH host-key policy and classify uncertainty conservatively;
- scans the explorer's seed directories—recent workload directories, configured roots, and the local `HOME`/remote `~` fallback—with fixed depth, filesystem-entry, result, wall-clock, output, and worker bounds; local and remote scans never follow symlinks, while remote scans use validated BatchMode SSH targets, POSIX-quoted root arguments, a fixed portable scanner, and a NUL-framed result protocol that rejects unsafe paths and malformed output;
- treats all `tether-*` names as reserved ownership space; only validated nonreserved names can enter the external attach type, duplicate/structurally malformed catalogs fail closed, and every external attach uses separated argv plus exact `=<name>` tmux targeting;
- does not collect or persist passwords, private keys, tokens, or telemetry;
- writes configuration and session metadata through advisory-locked, atomic transactions, with private directory/file permissions on Unix;
- inspects an established workload on explicit session close, persists a recoverable `closing` marker before killing an exact running session, and leaves indeterminate workloads unchanged; if initial metadata persistence fails, it attempts to kill the just-created workload and reports both failures plus the exact ID if rollback also fails. Pruning removes only sufficiently old metadata for sessions already proven absent or killed by explicit close; prune does not reconnect to or probe hosts.
- limits discovered external sessions to non-destructive catalog management and non-owning interactive attach; Tether never adopts, persists, renames, closes, kills, marks, or prunes them, and closing their Herdr view only detaches that client;

The trust boundary includes the local user account, the installed `ssh`, `tmux`, and Herdr executables, user-controlled configuration and command presets, and any selected remote host. Tether does not sandbox commands, secure a compromised endpoint, configure SSH trust, rotate credentials, or replace host authorization. Configuration can contain command presets; session metadata contains host targets, working directories, preset labels, identifiers, and timestamps. Protect the user account and backups accordingly.

Live strict-BatchMode verification from Hermes to `dev` covered remote create, real-TTY attach, detach, same-PID continuity, resume, exact close, prune isolation, and adversarial quoting without sentinel creation. Native Herdr placement interaction and macOS live lifecycle remain acceptance checks.

Security fixes are provided for the latest release. Older development snapshots are unsupported.
