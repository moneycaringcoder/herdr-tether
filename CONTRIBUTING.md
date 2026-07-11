# Contributing

Tether targets macOS and Linux. Install `tmux`, OpenSSH, and Rust 1.88 or newer with edition 2024 support. Herdr is needed only when exercising plugin integration.

## Development cycle

1. Start with a focused test that fails for the missing behavior or reproduced bug.
2. Make the smallest change that passes it.
3. Refactor without weakening observable assertions.
4. Exercise the affected command or integration path locally. Keep tests deterministic, isolated, and independent of network services.

Before submitting, run the same gates as CI from the repository root:

```sh
cargo fetch --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features -- --test-threads=1
cargo build --release --locked
cargo +1.88.0 check --locked --all-targets --all-features
```

Do not commit credentials, host details, generated state, or build output. Describe which local, SSH, tmux, and Herdr paths you actually exercised; call out paths that were not verified. Changes to process invocation, quoting, persistence, session lifecycle, or host handling need boundary and failure-path tests.

## Public documentation boundary

README, changelog, architecture, security, and plugin metadata describe the product and its supported behavior. Keep temporary planning, release coordination, test-machine details, review transcripts, local usernames, and local paths out of tracked product documentation. Use pull requests or untracked working notes for temporary coordination.
