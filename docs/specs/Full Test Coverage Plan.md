# Full Test Coverage Plan

The CLI's share of the coverage effort that brought the Kovalent apps to 100%. The same rules hold:

- **The target is 100%** of lines, regions and functions. Branch coverage needs nightly Rust, so it is not gated yet.
- **Exclusions are only for code that can't run in a test,** such as process entry points. Code that is merely hard to test is not excluded.
- **No coverage without a check.** A test asserts what a user or caller sees. Each new test is proven by breaking the code it covers and watching it fail.
- **Floors only rise.** Every PR raises them to CI's measured numbers, rounded down.
- **Unreachable code is removed, not excluded,** in its own commit with the reason.
- **A bug found while covering code gets its own commit,** with a test that fails first, and the PR names it.

Measure the way CI does: `cargo llvm-cov --locked --summary-only`. Tests bind loopback ports, so in a sandbox they need it disabled.

## Baseline (November 2026)

53.96% lines, 55.57% regions, 64.32% functions on macOS. The largest gaps: `nodes.rs`, `local.rs`, `device/mod.rs`, `selftest.rs`, `device/runner.rs` (0%), `login.rs` and `top.rs`.

Order: authentication and release verification first (`login.rs`, `config.rs`, `verify.rs`), then the device code, then the rest.

## PR 1: The coverage gate

**Goal:** CI measures coverage on every pull request and fails below the floors.

- The test step runs under `cargo llvm-cov` instead of `cargo test`, so the suite still runs once.
- Floors start at today's numbers, rounded down, and rise with each PR.
