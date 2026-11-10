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

## PR 2: Signing in

**Goal:** `login.rs` from 29% to all but its terminal prompts.

- **A scratch home for tests.** `config::ScratchHome` points one test thread's `~/.knaix` at an empty directory, so a test can save a session without touching the real one. Setting `HOME` would race every other test.
- **Seams, not mocks.** The browser flow takes the browser and the wait as arguments, and the terminal flow takes the code prompt, so tests drive the real callback server and the real HTTP calls against a fake API on loopback.
- **Cases:** a password sign-in, MFA with a spaced code, a refusal in the API's words, replies with no session, an API not listening, the browser callback saving the session, the wait expiring, and a terminal sign-in with no terminal.

Left for later: the three `dialoguer` prompts need a pseudo-terminal, which needs a dev-dependency. Two error arms cannot fire in a test: building the HTTP client failing, and the callback server stopping on its own.
