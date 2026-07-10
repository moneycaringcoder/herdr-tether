# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities through [GitHub private vulnerability reporting](https://github.com/moneycaringcoder/herdr-tether/security/advisories/new). Do not include exploit details, credentials, hostnames, or other sensitive data in a public issue. Include the affected version, platform, reproduction steps, impact, and any suggested mitigation. Maintainers will acknowledge the report and coordinate disclosure and remediation through the private advisory.

## Security model

Tether 0.1 supports macOS and Linux and manages durable `tmux` sessions locally or over ordinary OpenSSH connections. Remote operation is not native Herdr federation or remote pane streaming. Herdr integration opens a local plugin overlay and places resume commands in a split or tab.

Tether:

- invokes the system `ssh` client directly, with the remote target as a separate argument, and relies on OpenSSH authentication and the user's `known_hosts` and strict host-key policy;
- does not collect or persist passwords, private keys, tokens, or telemetry;
- writes configuration and session metadata atomically, with private directory/file permissions on Unix;
- inspects an established workload on explicit session close, kills only an exact running session, and leaves indeterminate workloads unchanged; if initial metadata persistence fails, it kills the just-created workload as rollback. Pruning removes only sufficiently old metadata for sessions already proven absent or killed by explicit close; prune does not reconnect to or probe hosts.

The trust boundary includes the local user account, the installed `ssh`, `tmux`, and Herdr executables, user-controlled configuration and command presets, and any selected remote host. Tether does not sandbox commands, secure a compromised endpoint, configure SSH trust, rotate credentials, or replace host authorization. Configuration can contain command presets; session metadata contains host targets, working directories, preset labels, identifiers, and timestamps. Protect the user account and backups accordingly.

Remote localhost smoke verification has not been completed: the attempted connection was rejected by strict host-key verification. This is not evidence that remote lifecycle behavior has been live-verified.

Only the latest development state is currently supported; no released version exists yet.
