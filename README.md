# Knaix CLI (v0.4.0)

The high-performance, single-tenant command-line daemon for Kovalent AI infrastructure. Engineered in Rust for memory safety, cryptographic isolation, and an uncompromising developer experience.

## Executive Overview

Knaix CLI inverts the traditional cloud AI paradigm. Instead of sending sensitive IP to a centralized cloud, Knaix securely bridges your local development environment directly into your dedicated node or EKS Pod via a zero-trust Tailscale mesh.

## Magic Developer Experience (DX)

*   **Agent Memory**: Intelligent, cross-session persistent state. Knaix intercepts explicit "Remember..." commands, appending facts to `_knaix_durable_memory.md` under `~/.knaix/memory/<node-id>` and uploading them to the node's knowledge base, while older context is compressed into `_knaix_ephemeral_log.md`.
*   **Terminal-Native Provisioning**: Execute `knaix up` to broker a compute request from the terminal. Infrastructure as Code abstracted directly to the CLI.
*   **Enterprise Scriptability**: Native `-o json` global flag compatibility across all telemetry and list commands, eliminating string-parsing friction in CI/CD pipelines. Standard stdout is dynamically structured utilizing the `comfy-table` engine.
*   **Recursive Ingestion**: The `knaix upload <path>` command leverages the highly optimized `walkdir` crate, seamlessly walking nested directories for massive bulk documentation embedding.
*   **Smart Failover**: Automatic health checks detect unreachable nodes and dynamically prompt interactive recovery sequences.
*   **REPL**: Persistent, markdown-rendered terminal sessions utilizing an intelligent local sliding window for context management.

## Security & Architecture

*   **Zero-Trust Mesh**: All network egress is routed strictly via an end-to-end encrypted WireGuard (Tailscale) tunnel. No inbound firewall ports required.
*   **Config Hardening**: Strict zero-trust configurations. The system enforces `0o600` POSIX boundaries on all configuration files.
*   **Atomic Saves**: Profile and configuration state mutations utilize the "Write-Sync-Rename" atomic pattern to guarantee token integrity against unexpected hardware halts.
*   **Persistent Connection Pooling**: The HTTP client maintains active connection reuse and TLS warming, radically reducing P2P latency.

## Try it locally, with no account

One command stands up the whole stack on your machine: the Node Runtime, its
own store, its own embedder, and its own reranker. There is no control plane,
no login, and no token. The image carries the model artifacts, so once it is
pulled nothing needs the network.

```bash
knaix local up
```

`knaix local up` fetches the image the first time it runs (about 380 MB) and
reuses it afterwards. Then point any command at the reserved node `local`:

```bash
knaix upload -n local ./docs
knaix chat -n local "what do these documents say about refunds?"
knaix use local          # make it the default for every command
knaix local status       # is it running and healthy
knaix local down         # stop it; the store is kept
```

Answers come from a deterministic mock unless you serve a model yourself and
pass `--llama-url`. Retrieval, reranking and citations are real either way, so
the part worth evaluating is the part that works out of the box.

## Installation

### Primary Install (macOS & Linux)
```bash
curl -sSL https://knaix.com/install.sh | sh
```

### Source Compilation (Requires Rust toolchain)
```bash
git clone https://github.com/kovalentai/knaix-cli.git
cd knaix-cli
cargo install --path .
```

## Quick Start Topology

1.  **Identity Bootstrap**:
    ```bash
    knaix login
    ```
2.  **Infrastructure Provisioning**:
    ```bash
    knaix up
    ```
3.  **Context Selection**:
    ```bash
    knaix use <node-id>
    ```
4.  **Inference & Interaction**:
    ```bash
    knaix repl
    ```

## CLI Reference

| Command          | Description                                           |
| :--------------- | :---------------------------------------------------- |
| `knaix login`    | Trigger SSO OAuth flow via Kovalent Identity Center.  |
| `knaix up`       | Provision a new Agent Node dynamically.               |
| `knaix list`     | List active nodes or browse ingested documents.       |
| `knaix use`      | Define the default node identity for rapid inference. |
| `knaix repl`     | Initiate an interactive conversation flow.            |
| `knaix chat`     | Dispatch a stateless, one-shot prompt.                |
| `knaix upload`   | Recursively ingest directories into vector storage.   |
| `knaix memory`   | Interrogate durable and ephemeral node memories.      |
| `knaix local`    | Run the whole stack on this machine (`up`, `down`, `status`, `logs`). |
| `knaix selftest` | Check that a node retrieves and cites correctly, against a bundled corpus. |
| `knaix status`   | Show the local configuration and whether a session exists. |
| `knaix metrics`  | Fetch a node's current health and latency.            |
| `knaix logs`     | Fetch the most recent log lines from the agent pod.   |
| `knaix config`   | Show or set the API URL used by the CLI.              |

**Global Flags:**
- `-o json`, `--output json`: Emit structured JSON instead of formatted tables.
- `--version`: Output the current installed binary version.

## Headless Execution

Knaix supports the following environment overrides for automated scripting:
- `KNAIX_TOKEN`: Kovalent API Bearer Token.
- `KNAIX_API_URL`: Override the control plane endpoint (default: `https://api.kovalentai.com`).
- `KNAIX_NO_UPDATE_CHECK`: Set to `1` to disable the daily version check, the only network request Knaix makes on its own behalf.
- `KNAIX_LOCAL_IMAGE`: Node Runtime image `knaix local` runs (default: `ghcr.io/kovalentai/node-runtime:latest`). Point it at a tag you built yourself to run that instead.

Values supplied through the environment apply to the running command only. They are never written to `~/.knaix/config.json`, so an ephemeral CI token does not outlive the job it was issued for.

---

<div align="center">
  <small>&copy; 2026 Kovalent AI &amp; Knaix. Licensed under the Apache License, Version 2.0.</small>
</div>
