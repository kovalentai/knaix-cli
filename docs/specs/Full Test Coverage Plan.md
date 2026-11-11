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

## PR 6: The running device

**Goal:** `device/runner.rs` and `device/sandbox.rs`, the long-running half of a device and the choice of sandbox for rpc-server, to all but what cannot run in a test. `device join` follows in PR 7, using these.

- **The OS and the programs are arguments.** The sandbox chooser takes the OS, how to find a program and how to probe a plan, so the macOS, Linux and unsupported branches all run on either platform, and no test starts a real `systemd-run` or `bwrap`. Stopping a leftover systemd unit takes the OS and `systemctl` the same way.
- **One run of rpc-server is split from finding it.** Starting the plan, waiting for it to answer, and stopping it are their own function, driven in tests by `sh` and `sleep` with the test holding the port. Checking the binary and choosing the sandbox stay with the real install.
- **The heartbeat stops on a future it is given.** `run` passes the shutdown signal; tests pass a channel, so a test never signals its own process. Waking the loop early, as rpc-server's changes do, makes the beats fast.
- **Through the binary,** `knaix device run` is refused until each precondition holds, then runs until the control plane refuses its heartbeat, and the service leaves the removed marker.
- **Cases:** each sandbox picked or refused on each OS, a probe that truncates the plan before rpc-server, renames the systemd unit and drops `--pipe`; the tailnet address waited for and given up on; rpc-server that answers, exits before it does, or is stopped, with its output in the log; heartbeats that succeed, see the ticket key rotate once, offer nothing, fail and back off, are refused, or find the tailnet address changed.

## PR 7: Joining a device

**Goal:** `knaix device join` in `device/mod.rs`, which turns a machine into a device: it checks the session, picks the hardware, installs llama.cpp, finds a sandbox, joins the tailnet, enrols and starts. All but the calls that need the real machine are covered.

- **Split by job.** Four parts move out of `join` into their own functions: forgetting a device the account no longer lists, joining the tailnet, enrolling (or updating the existing record), and installing the background service. Each takes what it needs, so tests drive them in-process with the fake control plane and a stand-in `tailscale`.
- **The prompt and `sudo` are arguments.** Joining the tailnet takes how to ask the owner and what to elevate `tailscale up` with, so the decline and the refusal run without a terminal, and no test runs `sudo`. The service install takes the service manager, so the launchd and systemd branches run with a stand-in on either platform.
- **What stays in `join`** is the glue to the machine: the hardware probe, the llama.cpp install and the sandbox probe. Through the binary, `join` is refused before any of that until the session, port, memory cap and backend are right, and says when this machine already is a running device. With an empty PATH it then runs as far as the machine allows: on macOS on to serving, on Linux it stops at the sandbox before anything is enrolled.
- **Cases:** a device the account still lists, no longer lists, or cannot be asked about; a machine already carrying the compute tag; the account's own tailnet, connected or not; Kovalent's tailnet joined with the key, after the owner agrees to switch from another tailnet, declines, or cannot be asked; a join that does not get the tag, and no IPv4 address; an existing record updated for this control plane and refused for another; a new enrolment saved with its warnings explained; and an enrolment without a usable ticket key taken back out.

## PR 8: The local node's container

**Goal:** the container half of `knaix local` in `local.rs`: `up`, `down`, `reset`, `status` and `logs`, with waiting for the node and warming it. `local setup` (an interactive picker) and `local connect` / `disconnect` (the relay to the account) follow in PR 9.

- **Through the binary, with no new seams.** Every docker call runs `docker` from the PATH, so a stand-in `docker` that keeps the container's state in files plays the daemon, and a loopback server on the published port plays the node. HOME is a scratch directory, so `local.json` and the default node are the test's own.
- **Cases:** Docker missing, and installed but stopped; a first start that pulls the image, publishes the node to loopback only, waits for it and warms it; a start against a model server on this machine's loopback, rewritten so the container can reach it, remembered for the next start, and a model that refuses the warm-up; a pull that fails with and without a copy already here; a node that exits while starting; a second `up` on a running node, with and without model flags; a node published on every interface re-created on loopback with its identity kept; a container of the same name that this CLI did not start; `down` with and without `--purge`; `reset` refused without a terminal, and run with `--yes` keeping the model choice; `status` and `logs` as text and JSON.

## PR 9: Choosing a model, and connecting a local node

**Goal:** the rest of `local.rs`: `knaix local setup`, which picks what answers, and `local connect` / `disconnect` with the relay that sends the node's metrics and logs to the account.

- **Unit tests never reach a real docker.** In a test build, `docker` is a stand-in the test names, or a path that does not exist, so a unit test cannot inspect or remove a container on the machine running it. The same thread-local pattern as `ScratchHome`.
- **Setup asks through an interface.** The prompts (pick one, type a value, yes or no) go through `Ask`: the terminal in use, scripted answers in tests. Discovering servers stays in `setup`, which passes what it found, so no test depends on what listens on this machine's ports.
- **The relay takes its pause and when to stop.** `connect` passes 15 seconds and never; tests pass milliseconds and a future that ends after two pushes. The login snapshot takes the context, so a test gives it the fake control plane.
- **Through the binary:** `connect` refused without docker, a node, a running container or a session; `connect --daemon` registers and starts a worker that relays until `disconnect` stops it and marks the node offline.
- **Cases:** each picker choice (the mock, a found server, a server typed in that answers or does not), models hosted elsewhere set aside, models ordered by fit with a warning for one too large, a model that will not answer kept or dropped, backing out at each prompt, and the node restarted, started or left as it is, with the default offered when another node holds it.
