# Knaix CLI Changelog

All notable changes to the Knaix CLI will be documented in this file.

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
