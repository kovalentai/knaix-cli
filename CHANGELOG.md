# Knaix CLI Changelog

All notable changes to the Knaix CLI will be documented in this file.

## [Unreleased]

### Security
- **Environment credentials stay ephemeral**: `KNAIX_TOKEN` and `KNAIX_API_URL` are no longer written into `~/.knaix/config.json`. Previously any command that saved the config, including the daily update check, persisted them to disk in plaintext, so a CI token outlived the job it was issued for.
- **Dependency advisories cleared**: Advanced `clap`, `reqwest`, and `axum`, resolving four RustSec vulnerabilities. The notable ones were certificate name-constraint bypasses and a CRL parsing panic in `rustls-webpki` 0.101, on the TLS path every command uses. CI now fails on any new advisory.

### Added
- **`KNAIX_NO_UPDATE_CHECK`**: Set to `1` to disable the daily version check, the only network request the CLI makes on its own behalf.

### Fixed
- **`knaix up` reports real state**: The command no longer pads its output with an invented five-second boot sequence. It returns when the API accepts the request and tells you how to watch for the node coming up.
- **Update check no longer clobbers concurrent writes**: It re-reads the config before saving, instead of overwriting it with the copy loaded at startup.
- **`knaix status`** pointed at a nonexistent `knaix select` command when no default node was set.

### Documentation
- Corrected the documented global JSON flag (`-o json`, not `--json`), the descriptions of `status`, `metrics`, `logs`, and `config`, where agent memory is written, and a reference to PKCE in a login flow that does not use it.

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
