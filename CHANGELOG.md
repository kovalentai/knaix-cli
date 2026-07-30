# Knaix CLI Changelog

All notable changes to the Knaix CLI will be documented in this file.

## [0.4.6] - 2026-07-30

The release that makes `knaix` usable by something other than a person typing it: documented exit codes, a project file, stdin, `--quiet`, and two commands for when you need to know why it broke or how fast it is.

### Added

- **`knaix doctor`.** Every other command stops at the first thing it finds wrong, so working out why nothing runs meant several commands and some guessing. This runs every check and reports all of them, with the command that fixes each one it did not like: the CLI version, `.knaix.toml`, the API URL, the control plane, your session, Docker, the local node, and whether the node your commands address can actually answer. One rule decides the exit code, and it is the rule that makes the command safe to put in CI: doctor fails when something on the path to your node is broken, and warns about anything that is not on it. A machine with no Docker is fine if your default node is hosted, and an unreachable control plane is fine if your default node is `local`, so neither is reported as a failure to someone it does not affect. A run that only warns exits 0. It is also one of two commands that survive a `.knaix.toml` they cannot parse, the other being `knaix init`, so the file that breaks every other command is reported as a finding rather than taking the diagnosis down with it.
- **`knaix bench`.** Where `selftest` answers whether a node answers correctly, this answers how long it takes. Three phases, because they slow down for different reasons and one end-to-end number hides which one moved: reach, a round trip to the node's health check; ingest, one generated document parsed, chunked, embedded and written; and answer, split into time to first token and total. That split is the useful one. Retrieval, reranking and prompt assembly all happen before the first token arrives, so a high first-token time with a small gap to the total means the knowledge base is slow, and the reverse means the model is. The document it uploads is part of the corpus while it is there, so a normal run removes it, `--sweep` clears what an interrupted run left on a hosted node, and the two outcomes that cannot remove it say so rather than exiting quietly.
- **Documented exit codes.** A script calling `knaix` could not tell "the node said no" from "the node was not there": every failure exited 1, so the only way to distinguish them was to parse English error text, which changes without warning. Eight codes now, published in the README and fixed as interface: 0 ok, 1 error, 2 usage, 3 auth, 4 unavailable, 5 not found, 6 denied, 7 precondition. The distinction worth having is 4 against 3 and 6. A 4 is worth retrying because nothing answered; a 3 or a 6 is not, because something answered and refused.
- **`.knaix.toml`, written by `knaix init`.** Which node a repository belongs to, and which of its files are worth ingesting, are properties of the repo rather than of your machine, so they belong in the repo and under review with everything else. The file is found by walking up from the working directory, the way git finds its root, because commands are run from subdirectories. A node is chosen by flag, then the file, then the saved default: the flag wins because it was typed for this one command, and the file beats the machine's default because it is the more specific statement.
- **Reading arguments from a pipe.** `knaix chat -` takes the question from standard input and `knaix upload -` takes the document, with `--name` deciding what piped content is cited as. Turns the CLI into a pipeline component rather than only an interactive tool.
- **`--quiet`.** Suppresses progress and commentary, keeps results, and never hides an error, so a script can use it without making itself less safe than one that did not.
- **A Homebrew formula on each release.** `brew install kovalentai/tap/knaix` now works alongside the curl installer. The renderer refuses to emit a formula it cannot back: it requires the release feed to already name the target, downloads all four binaries, and recomputes each SHA-256 from the bytes rather than trusting the sidecar.

### Changed

- **The REPL prompt is painted in the brand gradient**, along with the wordmark inside the line as you type it, matching what the zsh integration already does at the shell prompt. The prompt was the one place a REPL user looks at most and the one place the gradient never reached, because colouring the string handed to `readline()` was assumed to break line editing. It does not for the version pinned here, and the `Highlighter` route keeps the string rustyline measures and the string it prints from having to agree by coincidence.
- **`knaix upload --dry-run` needs no node and no account.** Which files an upload would send is decided by the directory and the filters and nothing else, so planning is its own step now. The preview cannot drift from what would actually happen, and a preview no longer fails against a node it never needed.
- **Durations read in seconds above a second.** `bench` and `selftest` share one format, so `13796ms` reads `13.8 s` and stops making you divide before you can react to the number.

### Fixed

- **`knaix upload no-such-path` says the path was not found.** It used to resolve a node before checking the path, so a typo was reported as "Not logged in".
- **An available upgrade no longer breaks `--json`.** The update banner was written to stdout after the command's document, so `knaix -o json list | jq` failed outright on any machine that had seen a newer release. It is suppressed for `--json` and `--quiet`.

### Removed

- **An installer that could never run.** The copy of `install.sh` in this repository resolved versions from the GitHub releases API and pulled tarballs from release assets, both of which 404 unauthenticated against a private repo, so it worked for nobody. Nothing pointed at it. The single installer is the one served from knaix.com.

## [0.4.5] - 2026-07-28

### Added
- **`knaix mcp`.** The Node Runtime speaks the Model Context Protocol, so Claude Code, Claude Desktop, Cursor and anything else that speaks it can search a node's knowledge base, ask it grounded questions, list its documents and add to them. What stood between you and that was a URL and a key in the right JSON shape. This prints it, filled in. The output is organised by the three shapes a client asks for its config in, a command, an HTTP server object, and a stdio bridge for clients that only launch local processes, with clients named as examples rather than as a list that would be wrong the week a new one appears. Against a local node it mints a key and installs it, so the printed block works as it stands; against a hosted node it prints the real address with a placeholder key, and says plainly that the address is on your tailnet and the key comes from the dashboard. Needs a node running the 0.29.0 runtime or newer; an older local node is told so, and given the command that fetches the current one, before anything is minted.
- **`knaix shell-init zsh`.** An opt-in shell integration that renders the knaix wordmark in the brand gradient as you type it. `--install` adds it to your profile after showing exactly what will change, `--uninstall` takes it back out, and both are idempotent through a fenced marker block. Install backs the profile up before touching a file it did not write. What the profile gets is an `eval` line rather than the snippet itself, so upgrading the CLI upgrades the integration and no dotfile can hold a stale copy. zsh only: bash and fish cannot colour individual characters of the line being typed, so there is nothing honest to ship for them yet.

### Changed
- **A node on your own tailnet reads as `BYO TAILNET` in `knaix list`**, not `SOVEREIGN (BYOT)`. The distinction it drew is real, that node is on your tailnet rather than our managed mesh, so the label names what is actually different instead of reaching for an adjective, and it matches the badge the dashboard shows for the same node.

### Fixed
- **Terminal colour depth is detected from more than `COLORTERM`.** That variable is the only one meant to carry 24-bit support and plenty of capable terminals never set it, so the wordmark fell through to the 16-colour branch and emitted a single cyan, which many themes render distinctly green. Detection now consults an explicit `KNAIX_COLOR` override, `NO_COLOR`, `CLICOLOR_FORCE`, `COLORTERM`, the terminfo `-direct` convention, a `TERM_PROGRAM` allowlist, known `TERM` values, and finally 256-colour.

## [0.4.4] - 2026-07-23

### Added
- **`knaix local connect` / `knaix local disconnect`.** Connect a running local node to your Kovalent account and it appears in the dashboard next to hosted nodes, with its own health, metrics and logs. The node stays offline: the logged-in CLI relays a metrics sample and the container's new log lines on an interval, so nothing on the node itself talks to the control plane. `--daemon` keeps relaying in the background; `disconnect` stops it and marks the node offline. `knaix login` connects a running node automatically, and `knaix logout` disconnects. Connecting a local node is a Community-tier offering, so it needs an account but no paid plan.

## [0.4.3] - 2026-07-23

### Added
- **`knaix local reset`**: empty the local store and start fresh in one command, keeping the model you picked. `down --purge` also clears the store but forgets the model and leaves the node stopped; `reset` is the front door for "clear what I ingested and let me start over", and it leaves the node running against an empty store. It confirms first, or takes `--yes` in a script.

### Changed
- **The local node becomes your default when you have none.** The first `knaix local up` (or `knaix local setup`) points later commands at `local`, so `knaix chat "..."` and `knaix upload ./file` work with no `-n local`. A default you already chose is kept, a hosted one included; the command then reminds you to pass `-n local` for the local node.
- **`knaix local setup` starts the node when it is not running.** Picking a model, or the mock, now offers to stand the whole local stack up, so a first run is a single `knaix local setup` rather than `up` followed by `setup`. A running node is still restarted so the pick takes effect.
- **A `/remember` note is named as your own note in citations.** In the "Grounded in" list a saved note reads as `your saved note (/remember)` rather than the internal `_knaix_durable_memory.md`, so grounding that came from a note you saved is recognisable as yours. The `[n]` marker is unchanged.

## [0.4.2] - 2026-07-23

### Added
- **Grounded, complete answers from the local node.** `knaix chat -n local` and the REPL send the node a grounding prompt, so an answer leads with the direct answer, adds the supporting detail and exceptions, and cites the passages it used with `[n]` markers, instead of a single terse line.
- **Multi-turn REPL.** The REPL carries the conversation so far to the node, so a follow-up is answered in the context of what came before. History is bounded to a recent character budget. `/reset` forgets it and starts fresh.
- **Answer length control.** `knaix chat --brief` and `--detailed`, and the REPL commands `/brief`, `/normal` and `/detailed`, choose how much detail an answer carries. On a hosted node the flags do not apply, and the command says so rather than dropping them silently.
- **Streaming answers from the local node.** A local answer prints token by token as the model writes it, over the node's streaming endpoint. A node image that predates the endpoint is detected and the CLI falls back to the one-shot request, so a CLI ahead of the node still answers.

### Changed
- **The README follows the documentation register.** Corrected claims that were inaccurate rather than merely energetic (knaix is a command-line client, not a daemon; dropped unearned performance language), aligned the command reference with each command's real behavior, and added the missing `logout` row.
- **One phrasing for an unreachable control plane**, matching the local-node error voice.

### Fixed
- **`knaix local up` no longer silently drops model flags against a running node.** `--mock`, `--model-url` or `-m` on an already-running node now warns that the node keeps its current model until it restarts, and names the two ways to apply the change, instead of printing the same line as a bare `up` and ignoring the flags.

## [0.4.1] - 2026-07-22

### Added
- **`knaix local setup`**: an interactive picker that probes the ports Ollama, LM Studio, vLLM and llama-server listen on, lists the models each server hosts, and remembers the choice. It offers to restart a running node so the pick takes effect immediately.
- **`knaix logout`**: removes the saved session from this machine, and points out a `KNAIX_TOKEN` still set in the shell.
- **`knaix completions <shell>`**: prints a shell completion script for bash, zsh, fish, powershell or elvish, generated from the parser so it cannot drift from the real flags.
- **`chat -o json`**: the answer, every retrieved passage with a `cited` flag, and the model that produced it, as structured output.
- **Directory upload filtering**: `knaix upload <dir>` skips directories that are never documentation (`.git`, `node_modules`, `target`, `dist`, virtualenvs) and files the node has no parser for, instead of sending them to be refused. `--include` and `--exclude` take globs, `--dry-run` shows what would be sent, and `--all` turns both defaults off. One unreadable file no longer abandons the run: the rest upload, the failures are named at the end, and the exit code is non-zero.

### Changed
- **`knaix local up` takes `--model-url` and `-m`/`--model`** (the earlier `--llama-url`/`--llama-model` remain as aliases). A loopback model URL is rewritten so the node's container can reach a server on your machine, and a model named without a server now warns rather than failing later.
- **Mock answers are labeled at every layer.** The answer is prefixed, a footer follows it, and JSON reports the model as `mock`. The "Grounded in" list shows only the passages an answer actually cited.
- **`-n` selects the node on `metrics`, `logs`, `repl`, `selftest` and `memory`**, the same as `chat` and `upload` already accepted.
- **`knaix login` times out after five minutes** instead of waiting indefinitely, and prints the sign-in URL when no browser opens.
- **Help text rewritten** so each command's one-line summary states plainly what it does.

### Fixed
- `metrics`, `logs` and `memory` against a local node no longer reach for the control plane; they read from the node itself.

## [0.4.0] - 2026-07-22

### Added
- **`knaix local`**: run the whole stack on your machine with no account, no token, and no control plane. One command starts the Node Runtime with its own store, embedder and reranker; the image carries the model artifacts, so once it is pulled nothing needs the network. `up`, `down`, `status` and `logs`. The image is fetched automatically on first run and reused afterwards.
- **`knaix selftest`**: check that a node answers correctly rather than that it merely responds. Ingests a bundled synthetic corpus, asks questions whose supporting passages are known in advance, and reports hit rate, MRR and citation accuracy against floors. Everything it uploads is deleted before it returns.
- **`KNAIX_NO_UPDATE_CHECK`**: set to `1` to disable the daily version check, the only network request the CLI makes on its own behalf.
- **`KNAIX_LOCAL_IMAGE`**: override the Node Runtime image `knaix local` runs, for anyone running a build of their own.

### Changed
- **The CLI talks to the native stack.** `upload`, `chat`, `repl` and document listing now use the native knowledge and chat endpoints instead of the AnythingLLM proxy, which had stopped existing: `upload` returned HTTP 500 and `chat` returned 404 against a current node. Answers now arrive with the passages they were grounded in, and node identifiers resolve by name, instance id or UUID.
- **`knaix login` follows your API URL.** The browser is sent to the host the configured control plane lives on, so a local or self-hosted deployment authenticates the same way as production.

### Fixed
- **Environment credentials no longer persist.** `KNAIX_TOKEN` and `KNAIX_API_URL` were written into `~/.knaix/config.json` by any command that saved the config, so the documented CI pattern left a bearer token on disk in plaintext. Values from the environment now apply to the running command only.
- **A fresh install keeps the default API URL.** The first config write recorded an empty URL, so a new install talked to nothing until the URL was set by hand.
- **`knaix up` reports real state.** It padded its output with an invented five-second boot sequence after the API had already answered.
- **The image pull is visible.** `knaix local up` captured docker's output, so a first run went silent for several hundred megabytes.
- **The update check no longer clobbers concurrent writes.** It re-reads the config before saving, rather than overwriting it with the copy loaded at startup.
- **`knaix status`** pointed at a nonexistent `knaix select` command when no default node was set.

### Security
- Cleared four RustSec advisories by advancing `clap`, `reqwest` and `axum`, including certificate name-constraint bypasses and a CRL parsing panic in `rustls-webpki` 0.101, on the TLS path every command uses. CI now fails on any new advisory.

### Documentation
- Corrected the documented global JSON flag (`-o json`, not `--json`), the descriptions of `status`, `metrics`, `logs` and `config`, where agent memory is written, and a reference to PKCE in a login flow that has never used it.

## [0.3.3] - 2026-03-03

### Sovereign Agentic Memory
- **Persistent Context**: The CLI now durably persists cross-session context directly to your local isolated storage (`~/.knaix/memory`).
- **Background Compaction**: Silently distills REPL conversation history on a background thread using LLM summarization without blocking your input loop.
- **Explicit Storage**: Added `/remember` to selectively save facts and `/memory` to audit what your Sovereign Node knows about you.
- **Local Navigability**: `knaix memory` now seamlessly lists and reads local files with markdown rendering via the new `--file` flag.
- **Strict File Permissions**: Enforced explicit local storage security (`0o700` for memory directories, `0o600` for memory files).

### Aesthetic & Magic DX
- **Premium Reporting Layout**: Unified and polished all CLI output feedback across the application (using `Info:`, `Error:`, `✓`).
- **UX Transparency**: Login, metric reporting, update fetching, and uploads now present clear, elegant status indicators and spinners without emoji noise.
- **Async Execution Updates**: The CLI cleanly monitors for updates in the background on a 24-hour cycle and optionally alerts you to newer versions.

## [0.3.2] - 2026-03-03

### Added
- **Terminal-Native Provisioning (`knaix up`)**: Securely handshake with the Kovalent API to trigger EKS pod provisioning (Community tier) or EC2 dispatch (Pro tier) entirely from the command line, with a multi-stage loading spinner.
- **Smart Directory Uploads**: Upgraded `knaix upload <path>` to accept directories via recursive parsing for bulk RAG ingestion.

## [0.3.1] - 2026-03-03

### Added
- **Structured JSON Output (`--json`, `-o`)**: Added support for raw JSON output (`-o json`) across data-retrieval commands (like `list` and `metrics`) for programmatic CI/CD consumption without ANSI interference.
- **Responsive Tables**: Integrated `comfy-table` to render elegant ASCII/Unicode tables that automatically adjust column widths based on your terminal size constraints.

## [0.3.0] - 2026-03-03

### Added
- **Immersive REPL**: Introduced `knaix repl`, a continuous conversational session featuring persistent command history and rich Markdown response rendering.
- **Smart Failover**: Implemented intelligent node resolution. Commands now automatically trigger an interactive selector if the default node is offline, preventing workflow disruption.
- **Environment Overrides**: Added support for `KNAIX_TOKEN` and `KNAIX_API_URL` environment variables to support headless CI/CD and automation pipelines.
- **Byte-Rate Progress**: Enhanced the `upload` command with real-time byte-rate tracking and ETA progress bars.

### Security & Reliability
- **Atomic Save Pattern**: Configuration updates now use a "Write-Sync-Rename" loop, ensuring `config.json` is never left in a corrupted state during system failures.
- **Permission Hardening**: The CLI now auto-enforces strict `0o600` (Owner Read/Write) permissions on the configuration file to protect authentication tokens.
- **Connection Pooling**: Implemented a shared `KnaixContext` to manage persistent TCP connections and warm TLS sessions, reducing command overhead by approximately 150ms.

### Fixed & Refactored
- **Error Handling**: Migrated top-to-bottom to `anyhow::Result` for structured, context-rich error reporting.
- **Aesthetic Refinement**: Stripped all emojis and non-ASCII characters from terminal output to maintain a professional, minimalist "Distinguished" technical standard.
- **Single-Node Detection**: Improved the "Smart Default" logic to better handle accounts transitioning between multiple nodes.

## [0.2.1] - 2026-02-15

### Added
- **Live Logs**: Added `knaix logs` command to stream real-time system logs from agent containers.
- **Context Management**: Introduced `knaix use <node_id>` to set a persistent default node for interaction commands.

### Changed
- **Output Alignment**: Refined `knaix list` (alias `ls`) with column-aligned headers for better readability.

## [0.1.0] - 2026-02-14

### Added
- **Initial Beta**: First release of the Rust-based Knaix CLI.
- **Auth Flow**: Integrated browser-based SSO login via the Kovalent Identity Center.
- **Core Commands**: Support for `chat`, `upload`, `metrics`, and node listing.
