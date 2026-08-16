<!--
Thanks for contributing. Nothing here is meant to be a hurdle — delete any
section that does not apply. A one-line typo fix needs a one-line description.
-->

## What this changes

<!-- What the change does, and why. If it fixes an issue, link it. -->

## How it was verified

<!--
Which of these you did. The suite passing is necessary but often not
sufficient: this project's characteristic bug is a workload whose real state
and reported state have quietly diverged, which looks exactly like a correct
report until someone acts on it.
-->

- [ ] `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` are clean
- [ ] `cargo test --locked` passes
- [ ] There is a test that fails without this change
- [ ] Ran against a live Herdr session, with what I observed described below

<!-- If it changes what a user sees, paste the before and after. -->

## Commit subjects

- [ ] Subjects say what the change does to the product, in sentence form —
      not a `type:` prefix. See CONTRIBUTING.

## Safety

<!-- Delete whichever section does not apply. -->

If you touched `src/lifecycle.rs`, `src/orchestration.rs`, `src/state.rs`, or
`src/storage.rs`:

- [ ] Tether still only stops or restarts workloads it owns, and the test that
      proves it refuses somebody else's still passes
- [ ] A failed transition leaves the workload recoverable rather than stranded
      in an unverified state
- [ ] State writes still go through the lock, temp file, atomic rename and
      parent sync, with `0600` files and `0700` directories

If you touched `src/herdr_socket.rs` or `src/status.rs`:

- [ ] A read that fails for a transient reason is retried, not reported as a
      refusal — `ErrorKind::Interrupted` included
- [ ] An unknown or future Herdr agent state is tolerated rather than treated
      as an error
- [ ] Uncertainty is still reported as uncertainty, and never as good news

If you touched `src/quote.rs`, `src/sshcfg.rs`, `src/config.rs`, or
`src/paths.rs`:

- [ ] Nothing user-supplied reaches a shell unquoted
- [ ] SSH config parsing still refuses includes that escape `~/.ssh`
- [ ] No secret, key material, prompt text, or terminal content enters state or
      logs
