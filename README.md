# Knaix CLI (v0.3.3)

The high-performance, single-tenant command-line daemon for Kovalent AI infrastructure. Engineered in Rust for memory safety, cryptographic isolation, and an uncompromising developer experience.

## Executive Overview

Knaix CLI inverts the traditional cloud AI paradigm. Instead of sending sensitive IP to a centralized cloud, Knaix securely bridges your local development environment directly into your dedicated node or EKS Pod via a zero-trust Tailscale mesh.

## Magic Developer Experience (DX)

*   **Agent Memory**: Intelligent, cross-session persistent state. Knaix intercepts explicit "Remember..." commands, committing facts atomically to `_knaix_durable_memory.md` on the isolated node, while older context is compressed into `_knaix_ephemeral_log.md`.
*   **Terminal-Native Provisioning**: Execute `knaix up` to automatically broker compute requests and simulate the EC2/EKS boot sequence via immersive `indicatif` rendering. Infrastructure as Code abstracted directly to the CLI.
*   **Enterprise Scriptability**: Native `--json` global flag compatibility across all telemetry and list commands, eliminating string-parsing friction in CI/CD pipelines. Standard stdout is dynamically structured utilizing the `comfy-table` engine.
*   **Recursive Ingestion**: The `knaix upload <path>` command leverages the highly optimized `walkdir` crate, seamlessly walking nested directories for massive bulk documentation embedding.
*   **Smart Failover**: Automatic health checks detect unreachable nodes and dynamically prompt interactive recovery sequences.
*   **REPL**: Persistent, markdown-rendered terminal sessions utilizing an intelligent local sliding window for context management.

## Security & Architecture

*   **Zero-Trust Mesh**: All network egress is routed strictly via an end-to-end encrypted WireGuard (Tailscale) tunnel. No inbound firewall ports required.
*   **Config Hardening**: Strict zero-trust configurations. The system enforces `0o600` POSIX boundaries on all configuration files.
*   **Atomic Saves**: Profile and configuration state mutations utilize the "Write-Sync-Rename" atomic pattern to guarantee token integrity against unexpected hardware halts.
*   **Persistent Connection Pooling**: The HTTP client maintains active connection reuse and TLS warming, radically reducing P2P latency.

## Installation

### Primary Install (macOS & Linux)
```bash
curl -sSL https://knaix.com/install.sh | bash
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
| `knaix status`   | Verify API health, mesh routing, and authentication.  |
| `knaix metrics`  | Stream real-time node latency and allocation metrics. |
| `knaix logs`     | Subscribe to live service stdout from the agent pod.  |
| `knaix config`   | Manipulate local CLI overrides and preferences.       |

**Global Flags:**
- `--json`: Emit structured JSON instead of formatted tables.
- `--version`: Output the current installed binary version.

## Headless Execution

Knaix supports the following environment overrides for automated scripting:
- `KNAIX_TOKEN`: Kovalent API Bearer Token.
- `KNAIX_API_URL`: Override the control plane endpoint (default: `https://api.kovalentai.com`).

---

<div align="center">
  <small>&copy; 2026 Kovalent AI &amp; Knaix. Licensed under the Apache License, Version 2.0.</small>
</div>
