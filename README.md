# Knaix CLI (v0.4.8)

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

### Homebrew (macOS & Linux)
```bash
brew install kovalentai/tap/knaix
```
Brings `brew upgrade knaix`, `brew uninstall knaix`, and shell completions.

### Install script (macOS & Linux)
```bash
curl -sSL https://knaix.com/install.sh | sh
```
That script is the single installer, and it lives in `kovalentai/knaix-docs`
(`public/install.sh`). It is deliberately not duplicated here: a second copy
drifted from it once already, and a copy attached to a release would be frozen
at the version it shipped with.

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
| `knaix init`     | Write a `.knaix.toml` so a repository remembers its node and what to ingest. |
| `knaix repl`     | Start an interactive chat session with a node.        |
| `knaix chat`     | Ask a node one question and print the grounded answer.|
| `knaix upload`   | Ingest a file or directory into a node's knowledge base. |
| `knaix memory`   | List or read the notes saved with `/remember`.        |
| `knaix mcp`      | Print the MCP client config that points Claude Code, Claude Desktop or Cursor at a node. |
| `knaix local`    | Run the whole stack on this machine (`setup`, `up`, `reset`, `down`, `status`, `logs`), or `connect`/`disconnect` it to your account to see it in the dashboard. |
| `knaix doctor`   | Check everything a command needs, and say what to do about what is wrong. |
| `knaix report`   | Write a diagnostic bundle you can read, then attach to an issue. |
| `knaix bench`    | Measure how fast a node reaches, ingests, and answers.  |
| `knaix selftest` | Check that a node retrieves and cites correctly, against a bundled corpus. |
| `knaix completions` | Print a shell completion script (bash, zsh, fish, powershell, elvish). |
| `knaix status`   | Show who is logged in, the default node, and the local node's state. |
| `knaix metrics`  | Show a node's health and latency.                     |
| `knaix logs`     | Show a node's recent log lines.                       |
| `knaix config`   | Show or set the API URL used by the CLI.              |

**Global Flags:**
- `-o json`, `--output json`: Emit structured JSON instead of formatted tables.
- `-q`, `--quiet`: Suppress progress and commentary. Results and errors still
  print, so a script can use it without hiding a failure.
- `--version`: Output the current installed binary version.

### Project settings (`.knaix.toml`)

Which node a repository belongs to, and which of its files are worth ingesting,
are properties of the repository rather than of your machine. `knaix init`
writes them down so they can be reviewed with everything else:

```bash
knaix init --node-id acme-prod --include 'docs/**/*.md' --exclude '**/CHANGELOG.md'
```

```toml
# How this repository talks to Kovalent.
# Commands run anywhere under this directory read this file.

# The node these commands address. A --node-id flag still wins.
node = "acme-prod"

[upload]
# Which files 'knaix upload .' ingests. Flags replace these, not add to them.
include = ["docs/**/*.md"]
exclude = ["**/CHANGELOG.md"]
```

The file is found by walking up from the working directory, the way `git` finds
its root, so commands work from a subdirectory. A node is chosen in this order:
a `--node-id` flag, then `.knaix.toml`, then the default set by `knaix use`.
Globs replace rather than merge, since narrowing for one command is the reason
to pass them.

A `.knaix.toml` that cannot be parsed stops the command rather than being
ignored. Running under different settings than the file asks for, while the file
looks correct, is the worse failure.

### Reading from a pipe

`-` means standard input, so `knaix` composes with the rest of the shell:

```bash
git log --since=1.week --oneline | knaix chat -
```

```bash
generate-report | knaix upload - --name weekly-report.md
```

`--name` sets what piped content is filed under, which is what citations will
show. It must be a plain file name, not a path. Only a bare `-` is special; a
path that merely starts with one is a path. Empty input is refused with exit
code 2, because the usual way to reach it is a pipeline whose first stage
produced nothing.

### Previewing without a node

`--dry-run` reports what an upload would send. Which files qualify is decided by
the directory and the filters, so it needs no node and no account:

```bash
knaix upload . --dry-run
```

It runs the same planning code a real upload does, so the preview cannot drift
from what would actually happen.

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

The node has to be new enough to serve MCP. A local node running an older image
is told so, with the command that fetches the current runtime, rather than being
handed a config that fails later in your editor:

```bash
knaix local up --pull    # if 'knaix mcp' says the node predates the endpoint
```

It prints three shapes, because that is what clients differ on -- not on the
protocol, which they all speak. A command for clients that register servers
themselves (Claude Code); an HTTP server object for config files (Cursor,
Windsurf and most others use `mcpServers`, VS Code uses `servers`); and a stdio
bridge for clients that only launch local processes and cannot dial an HTTP
server, Claude Desktop among them.

A client whose format is none of the three needs only the URL and the key as an
`Authorization: Bearer` header.

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

## When something is wrong: `knaix doctor`

Every other command stops at the first thing it finds broken, which means
diagnosing a setup takes several commands and some guessing. `doctor` runs every
check, reports all of them, and prints the command that fixes each one it did
not like.

```bash
knaix doctor                 # the node your commands would address
knaix doctor -n acme-prod    # a particular node
knaix doctor -o json         # every check, for a script or a CI step
```

It checks the CLI version, `.knaix.toml`, the API URL, the control plane, your
session, Docker, the local node, and finally whether the node your commands
address can actually answer.

One rule decides the exit code: **doctor fails when something on the path to
your node is broken, and warns about anything that is not on it.** The path is
everything a command traverses to reach the node it addresses -- the project
file and the API URL that decide where it goes, the control plane and the
session that authorize it, and the node itself. What is off that path is a
warning: a machine with no Docker is fine if your default node is hosted, and an
unreachable control plane is fine if your default node is `local`, so neither is
reported as a failure to someone it does not affect. A run that only warns exits
0 and is safe to gate a CI step on.

The first failure supplies the code, and the checks run in path order, so a
script reads the earliest broken thing rather than a later symptom of it.

`doctor` is also one of only two commands that survive a `.knaix.toml` they
cannot parse -- the other is `knaix init`, which is how a broken one gets
replaced. Every other command reads that file before it runs, so a broken one
breaks all of them; `doctor` reports it as a finding instead.

## Reporting a bug: `knaix report`

`doctor` tells you what is wrong. `report` packages it up so someone else can
help.

```bash
knaix report                 # write the bundle, and say what it left out
knaix report --open          # also open a new issue, environment filled in
knaix report --forget        # drop the recorded failures instead
knaix report -o json         # the bundle to stdout
```

It collects your version and how you installed it, your OS, shell and terminal,
the full `doctor` diagnosis, which settings are present, recent failures, and
recent local node log lines.

**Nothing is sent anywhere.** The bundle is a file on your disk. You read it, and
you decide whether to attach it. This is deliberate and it is not going to
change: a command that posted your environment to us on the day something broke
would sit badly next to a product whose whole claim is that we cannot read your
data.

What it removes, every time:

| Kept | Removed |
| :--- | :--- |
| That a token is present, and its length | The token |
| A stable hash of your username and node names | The names themselves |
| `api.kovalentai.com`, when that is what you use | A control plane address that is not ours |
| A model server's scheme and port | Its host, unless it is loopback |
| A log line's timestamp, level, method and route | Everything else on the line |
| The shape of the command that failed | Its arguments |

The list is not a promise you have to take on trust: the bundle carries a
`redactions` section naming every field it changed and why, and the command
prints the same list when it finishes.

Failures are recorded as they happen, already redacted, in
`~/.knaix/diagnostics.jsonl`. It keeps the last 20 and forgets the rest.
`knaix report --forget` empties it. A panic is recorded there too, so a crash
that scrolled off your terminal is still reportable.

## How fast is it: `knaix bench`

`selftest` answers whether a node answers *correctly*. `bench` answers how long
it takes.

```bash
knaix bench                  # 5 runs per phase against the default node
knaix bench --runs 20        # more samples for a tighter p95
knaix bench --no-ingest      # measure answering only, against what is already there
knaix bench --sweep          # remove documents an interrupted run left behind
knaix bench -o json          # every timing, plus the raw samples
```

Three phases, because they slow down for different reasons and a single
end-to-end number hides which one moved:

| Phase | What it measures |
| :--- | :--- |
| Reach | A round trip to the node's health check: the floor everything else sits on. Against a hosted node this goes through the control plane, which probes the node itself, so the number includes that hop. |
| Ingest | One generated document parsed, chunked, embedded and written: the write side of the vector store. |
| Answer | A question, split into time to first token and total. |

The split in the answer phase is the useful one. Retrieval, reranking and prompt
assembly all happen before the first token arrives, so a high first-token time
with a small gap to the total means the knowledge base is slow, and the reverse
means the model is.

A node answering with the deterministic mock is labelled as such: retrieval and
ingest are still real, but those answer timings measure the mock and must never
be compared with a real model's.

### What `bench` leaves behind

The document it ingests is a synthetic handbook, and while it is on the node it
is part of the corpus: retrievable, and citable in real answers. So the command
is careful about it, and so should you be.

A normal run deletes it before returning, and `--keep` leaves it deliberately.
Two cases cannot delete it, and both say so loudly rather than exiting quietly:
a node that stores the document without returning an id, and an ingest that
fails after the node has already written it.

On a hosted node, `knaix bench --sweep` finds anything named `knaix-bench-*` and
removes it; a run that finds leftovers warns before it measures. A local node
keeps chunks and no document registry, so there is nothing to search: clearing a
stray document there means `knaix local reset`.

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

### Exit codes

Every command exits with one of these. They let a script tell a refusal from a
crash without parsing the error text.

| Code | Name | Means |
| ---: | --- | --- |
| 0 | Ok | The command did what was asked. |
| 1 | Error | Something failed and no more specific code fits. |
| 2 | Usage | The command line was wrong: unknown flag, missing argument. |
| 3 | Auth | Not logged in, or the credential was rejected. |
| 4 | Unavailable | A node or the control plane could not be reached. |
| 5 | NotFound | The node, document, or thread named does not exist. |
| 6 | Denied | Refused on purpose: a policy said no, or a confirmation was declined. |
| 7 | Precondition | The machine is not ready: no local node running, Docker absent. |

The distinction that matters most in a pipeline is 4 against 3 and 6. A 4 is
worth retrying, because the far end was not there. A 3 or a 6 is not: the far
end answered, and said no.

```bash
knaix chat "what changed this week?" || case $? in
  3) echo "log in first" ;;
  4) echo "node is down, retrying later" ;;
  *) echo "gave up" ;;
esac
```

Declining an interactive confirmation exits 0, not 6. Nothing failed and nothing
was done. The refusal codes are for the non-interactive case, where a script
asked for something it had not authorised: `knaix local reset` without `--yes`
exits 6 rather than deleting the store.

These numbers are part of the interface. Changing what one means is a breaking
change, and `tests/exit_codes.rs` asserts each of them against the real binary.

---

<div align="center">
  <small>&copy; 2026 Kovalent AI &amp; Knaix. Licensed under the Apache License, Version 2.0.</small>
</div>
