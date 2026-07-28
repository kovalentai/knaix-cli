# Knaix CLI (v0.4.2)

The command-line client for Kovalent AI, written in Rust. Ingest documents into a private AI node and ask questions of them, either on your own machine with no account or on a hosted node over a zero-trust mesh.

## What it does

Instead of sending your documents to a shared cloud service, Knaix keeps them on a node you control: run the whole stack on your machine with `knaix local`, or bridge your terminal into a hosted node or EKS Pod over a Tailscale mesh. Either way, retrieval, reranking and citations run on the node.

## Developer experience

*   **Agent memory**: `/remember` in the REPL appends a fact to `_knaix_durable_memory.md` under `~/.knaix/memory/<node-id>` and ingests it into the node's knowledge base, so later questions can retrieve it. Older conversation context is compacted into `_knaix_ephemeral_log.md`.
*   **Provision from the terminal**: `knaix up` requests a hosted node on your account and reports the instance id; `knaix list` shows it coming up.
*   **JSON output for scripts**: the global `-o json` flag emits structured output from the list and telemetry commands, so CI pipelines don't parse tables. Text output is rendered with `comfy-table`.
*   **Recursive ingestion**: `knaix upload <path>` walks a directory, skipping what is never documentation and files the node has no parser for, rather than sending them to be refused.
*   **Node resolution**: name a node by its name, instance id or UUID; when no default is set, the CLI offers an interactive selector rather than failing.
*   **REPL**: an interactive session with markdown-rendered answers and a local sliding window for context.

## Security & architecture

*   **Zero-trust mesh**: traffic to a hosted node is routed over an end-to-end encrypted WireGuard (Tailscale) tunnel. No inbound firewall ports are required.
*   **Config hardening**: on Unix, `~/.knaix/config.json` is written with `0o600` permissions so the session token is readable only by its owner.
*   **Atomic saves**: configuration writes use a write-sync-rename pattern, so an interrupted write cannot leave a truncated or corrupt token on disk.
*   **Connection reuse**: the HTTP client keeps idle connections warm with a pooled, keep-alive configuration, so back-to-back commands avoid a fresh handshake each time.

## Try it locally, with no account

One command stands up the whole stack on your machine: the Node Runtime, its
own store, its own embedder, and its own reranker. There is no control plane,
no login, and no token. The image carries the model artifacts, so once it is
pulled nothing needs the network.

```bash
knaix local setup
```

`knaix local setup` finds the model servers running on your machine (Ollama,
LM Studio, vLLM, llama-server), lets you pick one (or the deterministic mock),
and starts the node. The image is fetched the first time (about 380 MB) and
reused afterwards. The local node becomes your default, so the commands that
follow need no `-n local`:

```bash
knaix upload ./docs
knaix chat "what do these documents say about refunds?"
knaix local status       # is it running and healthy
knaix local reset        # clear everything ingested and start fresh
knaix local down         # stop it; the store is kept
```

Already running a model server, or want to skip the picker? Start the node
directly and name the server once; it is remembered, so later starts are just
`knaix local up`:

```bash
knaix local up                                                        # the mock, no model needed
knaix local up --model-url http://localhost:11434 -m qwen3.5:latest   # a model on this machine
```

`--mock` goes back to the mock deliberately. Retrieval, reranking and citations
are real either way, so the part worth evaluating is the part that works out of
the box.

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

## Quick start (hosted node)

1.  **Sign in** (opens the browser):
    ```bash
    knaix login
    ```
2.  **Provision a node**:
    ```bash
    knaix up
    ```
3.  **Set it as the default**:
    ```bash
    knaix use <node-id>
    ```
4.  **Ask it questions**:
    ```bash
    knaix repl
    ```

## CLI Reference

| Command          | Description                                           |
| :--------------- | :---------------------------------------------------- |
| `knaix login`    | Sign in through your browser, following your configured API URL. |
| `knaix logout`   | Remove the saved session token from this machine.     |
| `knaix up`       | Provision a hosted node on your Kovalent account.     |
| `knaix list`     | List your hosted nodes, or the documents on one node. |
| `knaix use`      | Set the default node for later commands.              |
| `knaix repl`     | Start an interactive chat session with a node.        |
| `knaix chat`     | Ask a node one question and print the grounded answer.|
| `knaix upload`   | Ingest a file or directory into a node's knowledge base. |
| `knaix memory`   | List or read the notes saved with `/remember`.        |
| `knaix mcp`      | Print the MCP client config that points Claude Code, Claude Desktop or Cursor at a node. |
| `knaix local`    | Run the whole stack on this machine (`setup`, `up`, `reset`, `down`, `status`, `logs`), or `connect`/`disconnect` it to your account to see it in the dashboard. |
| `knaix selftest` | Check that a node retrieves and cites correctly, against a bundled corpus. |
| `knaix completions` | Print a shell completion script (bash, zsh, fish, powershell, elvish). |
| `knaix status`   | Show who is logged in, the default node, and the local node's state. |
| `knaix metrics`  | Show a node's health and latency.                     |
| `knaix logs`     | Show a node's recent log lines.                       |
| `knaix config`   | Show or set the API URL used by the CLI.              |

**Global Flags:**
- `-o json`, `--output json`: Emit structured JSON instead of formatted tables.
- `--version`: Output the current installed binary version.

## Using a node from your editor

The node speaks the Model Context Protocol, so any MCP client can search its
knowledge base, ask it grounded questions, list its documents, and add to them.
`knaix mcp` prints the config, already filled in:

```bash
knaix mcp                # for the default node
knaix mcp -n <node-id>   # for a specific one
knaix mcp -o json        # just the config object, for piping into a file
```

Against the local node this also mints a key and installs it, so the printed
block works as it stands. Against a hosted node it prints the node's address
with a placeholder, because those keys are issued from the dashboard (Keys tab)
and the address is on your tailnet -- the machine running the client has to be
on it too.

It prints three shapes because clients differ in what they accept: a command
(Claude Code), an HTTP server object (Cursor and most others), and a stdio
bridge for clients that only launch local processes and cannot dial an HTTP
server, Claude Desktop among them.

What the client gets: `search_knowledge_base` returns source passages,
`ask_knowledge_base` returns an answer generated on the node with citations,
`list_documents` enumerates the corpus, and `ingest_document` adds to it. Each
document is also readable as a resource. A key's scopes decide which of those
the client sees.

## Ingesting a directory

`knaix upload` takes a file or a directory. A directory is walked recursively,
skipping what is never documentation -- `.git`, `node_modules`, `target`,
`dist`, virtualenvs and the like -- and skipping files the node has no parser
for, rather than sending them to be refused.

```bash
knaix upload ./docs                          # everything ingestible under ./docs
knaix upload . --dry-run                     # show what would be sent, send nothing
knaix upload . --include '*.md' --include '*.pdf'
knaix upload . --exclude 'CHANGELOG.md'
knaix upload . --all                         # override both defaults
```

`--include` replaces the type default, so asking for `*.rs` sends source files.
`--exclude` always wins. A bare pattern like `*.md` matches at any depth; one
containing a slash is matched literally against the path.

One unreadable file no longer abandons the run: the rest still upload and the
failures are named at the end, with a non-zero exit.

## Shell completion

```bash
knaix completions zsh  > "${fpath[1]}/_knaix"      # zsh
knaix completions bash > /etc/bash_completion.d/knaix
knaix completions fish > ~/.config/fish/completions/knaix.fish
```

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
