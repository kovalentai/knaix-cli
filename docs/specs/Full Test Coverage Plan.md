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
- **The README badge.** On each push to `main`, a separate job commits the line coverage, rounded down, as a shields.io endpoint file to the orphan `badges` branch, only when it changed. Only that job can write. It may fail without marking `main` red. Red below 60%, yellow below 80%, green from 80%.

## PR 2: Signing in

**Goal:** `login.rs` from 29% to all but its terminal prompts.

- **A scratch home for tests.** `config::ScratchHome` points one test thread's `~/.knaix` at an empty directory, so a test can save a session without touching the real one. Setting `HOME` would race every other test.
- **Seams, not mocks.** The browser flow takes the browser and the wait as arguments, and the terminal flow takes the code prompt, so tests drive the real callback server and the real HTTP calls against a fake API on loopback.
- **Cases:** a password sign-in, MFA with a spaced code, a refusal in the API's words, replies with no session, an API not listening, the browser callback saving the session, the wait expiring, and a terminal sign-in with no terminal.

Left for later: the three `dialoguer` prompts need a pseudo-terminal, which needs a dev-dependency. Two error arms cannot fire in a test: building the HTTP client failing, and the callback server stopping on its own.

## PR 3: Saving config and verifying a release

**Goal:** `config.rs` and `verify.rs` to all but what cannot run in a test.

- **Config:** a saved config reads back and is readable by its owner only, and a config that cannot be replaced is an error that leaves what is there alone.
- **Verify's seams.** The release host and the `cosign` and `gh` programs are passed in, so tests run the real downloads and processes against a loopback host and stand-in scripts. The platform naming takes the OS and architecture, so every arm is checked on any machine. The text and JSON reports and the closing line are built as values, so tests read what the user sees.
- **Cases:** the digest read, lowercased, or refused for each reason; the bundle fetched only when published; cosign asked for this repository's release identity; a missing attestation told apart from a bad one; a binary that passes every check, one that differs from the release, and a signature with no cosign to check it; a missing file exiting not-found.
- The stand-in scripts are Unix only, so the Windows job skips those tests.

Left: the permission-hardening warning in `save_config` cannot fire on a filesystem that supports permissions, and the test helper's retry for a script still open for writing runs only when that race happens.

## PR 4: The device's control plane, state and commands

**Goal:** `device/api.rs` and `device/state.rs` to all but what cannot run in a test, and the `status`, `ls`, `leave`, `rekey` and `logs` commands covered end to end. `join`, the runner and the sandbox follow in their own PR.

- **A shared fake control plane.** `testing::fake_api` answers `"METHOD /path"` routes and records each call's route, authorization and body, for any module's unit tests.
- **The commands run through the binary,** with HOME a scratch directory and PATH empty. `leave` and `rekey` stop services by asking launchd or systemd by name; with neither on the PATH, no test can stop a real device on the machine running it. A `sleep` process stands in for the running device, so stopping one is observed, not assumed.
- **Cases:** every route's path, session and refusals, including a removed device's heartbeat, a join key without the compute tag, an account on its own tailnet, and the ticket key's outage; the record and runtime files, their permissions, unreadable and unremovable records, log rotation, and the socket directory refusing a link or a path too long; status running, failing, stopped and removed; ls with and without devices or a session; leave with and without the account, with `--purge`, and unconfirmed; rekey pinning, refusing a key that is not what it claims, and unconfirmed; logs with ages.
- The command tests are Unix only, as device commands are.

## PR 5: What a device asks of the machine

**Goal:** `device/service.rs`, `device/tailnet.rs`, `device/hardware.rs` and `device/llama.rs`, the code that drives launchd and systemd, the Tailscale CLI, the GPU tools and the llama.cpp download, to all but what cannot run in a test. `join` and the runner follow, using these.

- **The OS is an argument, not a `cfg!`.** CI measures on Linux, where a `cfg!(target_os = "macos")` branch is compiled but never runs. The service manager and rpc-server's environment take the OS, so both platforms' branches run on either.
- **Programs are arguments too.** The service manager, `sudo`, the GPU probe and the release host are passed in, so tests drive stand-in scripts and a loopback host. `config::home()` follows `ScratchHome`, so the service's files land in the test's home.
- **Shared helpers.** `testing::fake_tool` writes a stand-in program that records each call's arguments, and `testing::serve_files` serves bytes by path. The verify tests now use both.
- **Cases:** the launchd agent and systemd unit written, loaded, restarted and removed, and refused; the stable link chosen over a versioned path; Tailscale found on the PATH first, its status read logged out, the join key passed only through a private file that is then deleted, with and without sudo; GPU detection's order, the driver floor and a probe that hangs; the pinned archive installing only what runs, a changed install refused and reinstalled, an archive with the wrong checksum installing nothing, and each failed download.
