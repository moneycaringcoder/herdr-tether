# Contributing

Tether targets macOS and Linux. Install `tmux`, OpenSSH, and Rust 1.88 or newer with edition 2024 support. Herdr is needed only when exercising plugin integration.

## Development cycle

1. Start with a focused test that fails for the missing behavior or reproduced bug.
2. Make the smallest change that passes it.
3. Refactor without weakening observable assertions.
4. Exercise the affected command or integration path locally. Keep tests deterministic, isolated, and independent of network services.

Before submitting, run the core local gates from the repository root:

```sh
cargo fetch --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features -- --test-threads=1
cargo build --release --locked
cargo +1.88.0 check --locked --all-targets --all-features
python3 -m unittest discover -s tools -p 'test_*.py'
python3 tools/check_docs.py
python3 tools/check_package.py --allow-dirty
```

The package check creates the exact source archive used by the remaining
packaged-public-contract tests. Release, plugin-manifest, Herdr-integration, or
live-smoke changes must also run the applicable commands from
`.github/workflows/ci.yml`, `.github/workflows/live-product.yml`, and
`.github/workflows/release.yml`; report the exact Herdr versions and platforms
exercised. `--allow-dirty` is for local review only—the committed CI package
gate runs without it.

Do not commit credentials, host details, generated state, or build output. Describe which local, SSH, tmux, and Herdr paths you actually exercised; call out paths that were not verified. Changes to process invocation, quoting, persistence, session lifecycle, host handling, Agent metadata, or view restoration need boundary and failure-path tests.

## Interrupted reads, writes, and waits

A signal delivered while a thread is blocked in a read, a write, or a wait
surfaces as `EINTR`. It describes no failure: the socket is still connected and
the child is still running, and the call has to be made again. Herdr's own TUI
raises `SIGWINCH` freely, so any blocking call on the socket, `tmux`, or SSH
paths can span one.

On those paths a bare `?` on a blocking read, write, or wait is a bug. Route the
call through `crate::interrupt::retry_interrupted` instead, and give it the bound
the call site already has: a relative socket timeout, the deadline of an
enclosing loop, or nothing to bound when the descriptor is non-blocking. A retry
loop with no bound turns a bounded wait into an unbounded one, because a relative
timeout restarts on every call.

A call that should propagate the interruption instead is a valid outcome, but an
argued one. `crate::interrupt` lists every such call on those paths with the
reason it does not retry, including the standard-library calls that already
retry internally. Add to that list rather than leaving the reasoning in a commit
message.

## Commit subjects

Write the subject as a sentence describing what the change does, in the
imperative mood, capitalized, with no trailing period and no `type:` prefix.
`Tolerate future Herdr agent states`, `Report refused prompts as refused, not
uncertain`, and `Prefer the invoking pane's sibling worktrees in the picker` are
the shape to follow. Squash-merging appends the pull request number, so
`(#68)` is added for you rather than written by hand.

Conventional-Commits prefixes such as `chore:`, `ci:`, `test:`, and `feat:` are
not used here. They classify the change by the kind of file it touches, which
the diff already shows, and spend the most readable part of the subject line on
it. A few commits carry them anyway; those predate this section and stay as
they are, because rewriting published history would break every tag, release,
and canary run that names an existing SHA.

The body, when a change needs one, explains why the change is right rather than
restating the diff. Do not add `Co-Authored-By` trailers, "Generated with"
lines, or any other tool or model attribution to commits, tags, or release
notes.

## Versioning and rollout

Before 1.0, `0.x.0` is the feature and breaking-change train. A `0.x.y`
release where `y` is greater than zero is a backward-compatible patch train
limited to fixes, corrective diagnostics, tests, documentation, CI, and
release hardening. State or configuration migrations, new commands, actions,
or capabilities, platform expansion, and a higher minimum Herdr version belong
in the next `0.x.0` release.

Every changed behavior needs a focused regression test, including its relevant
boundary or failure path. The current stable Herdr release is a blocking
integration gate; unreleased upstream builds remain an advisory canary until
their behavior ships in a stable release and its compatibility is reviewed.

Validate an exact candidate commit SHA through every applicable local and CI
gate before release. Creating the final tag requires explicit maintainer
approval. Keep temporary scope, sequencing, acceptance, and rollout
coordination in GitHub issues and pull requests rather than product
documentation.

## Public documentation boundary

README, changelog, architecture, security, and plugin metadata describe the product and its supported behavior. Keep temporary planning, release coordination, test-machine details, review transcripts, local usernames, and local paths out of tracked product documentation. Use pull requests or untracked working notes for temporary coordination.

`docs/roadmap.md` holds settled boundaries, decisions taken against building
something, and ideas that have not been built. An entry is retired in the change
that ships it, in the same pull request, so the file never describes a property
the code already has as though it were a plan. A change that delivers a roadmap
entry and leaves the entry in place is incomplete: a reader cannot tell the two
apart, and every stale entry makes the settled boundaries beside them read as
provisional too.
