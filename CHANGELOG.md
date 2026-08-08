# Knaix CLI Changelog

All notable changes to the Knaix CLI will be documented in this file.

## [0.6.0] - 2026-08-08

<!-- One paragraph on what this release is about. Delete this comment. -->

## [0.5.4] - 2026-08-07

One fix, for something every table in the CLI was quietly doing.

### Fixed

- **The tables line up again.** Every table here colours its cells, and comfy-table measures a cell over the raw string, escape sequences included. A coloured cell was therefore reckoned wider than it prints and padded short, so its row stopped well before the border while an uncoloured row in the same table reached it.

  `knaix metrics` showed it worst. Its label column is dimmed and three of the six values are coloured, so three rows ended early and three did not, which reads less like a rendering bug than like the data is ragged. `doctor`, `config`, `local status`, `bench`, `selftest`, `list` and `top` all had it.

  Columns come out narrower now too, because a width no longer counts bytes that never print.

## [0.5.3] - 2026-08-05

Answers arrive as they are written, say where they came from, and can be asked to be longer, shorter, or about one document. Most of this release is about the wait and the evidence rather than the model.

Two of the fixes below were found by running the CLI against a real model and a real corpus rather than by reading the code, and one of them was a regression introduced earlier in this same release.

### Added

- **`knaix repl` streams its answers.** The tokens were already arriving one at a time; the session held all of them and rendered the finished answer at the end, so the surface built for conversation was the one that showed you least. It now renders as the answer lands, a line at a time, and keeps the markdown: a fenced code block is held until it closes so it arrives whole.

- **The wait says what it is doing.** Both the node and the control plane hand over the retrieved passages before the first word of the answer. `Thinking...` became `Searching your documents...`, then the documents retrieval actually found. Past five seconds the spinner also counts, because a line that never changes reads the same whether the model is working or the connection has died.

- **`knaix local up` warms the node before handing it over.** The first question a cold node answered paid for loading the query embedder, the reranker, and the model's weights. On a real machine that was nineteen seconds, spent under whoever asked first. It is now spent once, at startup, where it is labelled and counted.

- **A hosted session is a conversation.** Every question used to be sent on its own and opened a new thread, so a follow-up had no idea what came before and your transcripts filled with one-message conversations. `knaix repl` now follows the thread the control plane opens. `/reset` starts a new one; the previous conversation is kept, not deleted.

- **`/source <n>` prints a passage in full.** The node returns each retrieved passage whole and the Grounded in list showed the first line or so, so the evidence behind a claim was arriving and being discarded at the point of display. A passage that was retrieved and never cited is reachable too, and labelled: that is what the model saw and did not use.

- **`--doc <name>` grounds an answer in a document you name.** Every question was a search of the whole corpus for its best few passages, which answers a lookup well and answers "summarize this" or "draw me a quiz from this" badly, because the passages that best match the word *summarize* are rarely the ones the task needs. Naming a document reads it in order instead. On a thirteen-chunk study guide the difference is five passages against thirteen. Needs a node new enough to serve it; an older one says so.

- **`--k <n>` chooses how many passages an answer rests on**, and `--doc`, `--brief` and `--detailed` now work in `knaix repl` too, both as flags that set where a session starts and as commands that change it as you go.

- **A slow answer says where the time went.** Every answer was already timed at its first word and its last, and only `knaix bench` ever read those numbers. Past five seconds the answer now reports how much of the wait was finding and how much was writing, which is the difference between a slow knowledge base and a slow model.

### Changed

- **`--brief` and `--detailed` change the answer.** They shaped a prompt and nothing else, so a detailed answer was still cut off at the node's default length whatever the prompt asked for, and against a hosted node the flags printed a note admitting they did nothing at all. They now carry both a shape and a length, on either kind of node. Measured on a real model, the same question answers in 53, 82 and 130 words.

- **A local node reranks every answer.** The cross-encoder runs on your own machine, behind the same boundary as everything else, so there was nothing to meter and no reason to leave the largest quality lever switched off.

- **`knaix repl --help` describes the session.** It listed three flags and nothing about the commands inside, so `/source`, `/k`, `/doc`, `/remember` and `/reset` could only be found by starting a session and guessing that `/help` existed.

### Fixed

- **Half of all answers showed no sources.** Turning on reranking, earlier in this same release, also handed the reranker the decision about what the model was allowed to read, against a fixed score floor. Its scores turn out to be bimodal rather than calibrated: near 1 when it is confident and near 0 when it is not, with everything below at a thousandth. So a confident reranker admitted exactly one passage and an unconfident one admitted none, which meant answers were better the less sure it was.

  One passage is worse than thin. Handed a single passage numbered `[1]`, a model writes a multi-point answer and numbers its own points `[1] [2] [3]`; every marker past the first matches nothing, so no passage is marked as cited and the Grounded in block disappears entirely. Against a real corpus, three of six answers cited nothing. All six do now. The reranker orders the passages; retrieval depth decides how many there are.

- **`/reset` said a hosted conversation had been cleared.** It starts a new thread and leaves the old one stored, so anyone resetting to take back a question was told the transcript was gone when it was not.

- **A flag that could not take effect said so.** `--k` against a hosted node, `--k` alongside `--doc`, and a hosted answer the control plane could not add to a conversation were all accepted in silence. Each now says what happened.

## [0.5.2] - 2026-08-04

Everything here came from watching a first run rather than from reading the code.

### Fixed

- **`knaix local setup` left you passing `-n local` to everything.** A default node you have already chosen is kept, deliberately, so a machine with a hosted node saved never handed the shorthand to the local one. That is the right rule and it was the wrong ending for setup: somebody who has just been asked which server should answer and which model to use is onboarding onto that node. Setup now asks whether to make it the default, once, defaulting to no. `knaix local up` still keeps its hands off, because starting a node is not a statement about which one you address.

- **A command that could not reach the control plane never mentioned the node on the same machine.** `knaix upload doc.md` against a hosted default with no route out printed a DNS chain and suggested `knaix doctor`. Both true, neither useful, while a local node sat running and would have taken the upload. The error now says so and names the two ways to use it.

- **The local node refused to do anything but look things up.** Asked to draw a quiz out of a study guide it had just ingested, it answered that the knowledge base held no information about administering quizzes. That was the prompt, not the model: it described a lookup and nothing else, so any request to work *with* the material was refused. Grounding is about where the facts come from, not about what may be asked for, and the two are stated separately now. Summaries, questions, outlines and comparisons all work, still built only from the context and still cited.

- **`knaix shell-init` and `knaix completions` refused to run without the shell named.** For `shell-init` that argument had exactly one legal value. For `completions` the process can read `$SHELL` itself. Both now default to the shell you are running, and naming one still works, which remains the only way to generate for a shell you are not in.

- **Installing on Windows failed at the checksum.** `irm https://knaix.com/install.ps1 | iex` stopped with "Could not download the checksum file" when the download had in fact succeeded. Already fixed for everyone, without a new release: the installer is served from the docs site, and the published checksums now carry the content type PowerShell needs.

## [0.5.1] - 2026-08-04

**If you ran `knaix verify` on v0.5.0, run it again.** It never checked the signature, on any platform.

### Fixed

- **`knaix verify` never checked a signature.** It looked for cosign by running `cosign --version`, and cosign spells that as a subcommand: `cosign --version` exits 1 with `unknown flag`. So the check concluded cosign was missing on every machine that had it, and reported `cosign is not installed, so the signature was not checked` while cosign sat on the PATH. Both spellings are now tried.

  It under-reported rather than over-reported. Nobody was ever told a signature had been verified when it had not, and `--strict` correctly refused to pass, because the check registered as skipped rather than passed. But the check itself did nothing, which on the release that introduced it is the whole point.

  The installers were never affected: `install.sh` finds cosign with `command -v` and `install.ps1` with `Get-Command`, and both are correct. Only `knaix verify` looked the wrong way.

## [0.5.0] - 2026-08-03

Knaix runs on Windows, and every release can now be checked against the workflow that built it.

**If you install with `curl … install.sh | sh`, read the Fixed entry.** On a machine with no hashing tool the installer reported a checksum it had not computed.

### Added

- **Windows.** There is a Windows build, installed from PowerShell with `irm https://knaix.com/install.ps1 | iex`. It installs to `%LOCALAPPDATA%\Programs\knaix` and puts that on the user PATH, so it never asks for administrator rights.

  The build is `x86_64-pc-windows-gnu`. Windows on ARM runs it under emulation, and the installer says so rather than refusing a machine that works. `knaix local` needs Docker Desktop, as it does everywhere else. `knaix local connect --daemon` is the one thing that does not work there: it says so plainly instead of failing halfway.

- **`knaix verify`.** Checks that the binary you are running is the one we published. It re-hashes the file and compares it with the digest published for that version, verifies the release signature with cosign, and checks the build attestation with the GitHub CLI.

  A check that could not run is reported as **skipped**, with the reason, and never counted as a pass. cosign and the GitHub CLI are optional, so their absence is stated rather than glossed over. `--strict` turns a check that could not run into a failure, which is what a pipeline wants. Exits 6 when a check fails and, under `--strict`, 7 when one could not run. `-o json` emits the same result as a document.

  With no arguments it checks the running binary against the version it reports, not against the newest release, so deliberately staying on an older version does not read as a failure. Checking some other file needs `--version`, because a file on disk does not say which release it came from and a guess produces a mismatch that looks like tampering.

- **Signed releases and build provenance.** Every published binary now carries a Sigstore signature and a SLSA build attestation. Signing is keyless: the release workflow proves its identity with a short-lived token, so there is no signing key to steal, rotate or expire. The certificate records which workflow, in which repository, at which tag produced the file, and every verification path pins all three. Releases before this one carry neither, so those checks report as skipped against them.

  An SBOM is published with each release.

### Fixed

- **The install script claimed a checksum it had not computed.** On a machine with neither `sha256sum` nor `shasum`, the installer printed `Checksum verified.` and installed the download anyway. It had hashed nothing: the missing-tool branch assigned the expected value to the actual one, so the comparison it printed that line for was between a value and itself.

  It now tries `openssl` as a third option, and if none of the three is present it stops, names them, and installs nothing. An installer that cannot verify a download is not the thing that should decide to go ahead. It also verifies the release signature when cosign is present, and says the signature was not checked when it is absent.

## [0.4.10] - 2026-08-01

### Fixed

- **Piping any command into `head` ended in a crash report.** `knaix top | head` and `knaix chat | head` printed a Rust panic, a backtrace note, and an invitation to file a bug, for doing the most ordinary thing anyone does with a stream. Rust ignores the signal that says a reader has gone, which turns writing to a closed pipe into an error the print macros panic on. The signal is now left alone, so the reader leaves and the writer stops, which is what every other tool in a pipeline already does.

  This was every command that writes more than fits in a pipe buffer or keeps writing over time, not only `top`. A command ended this way exits 141, the shell's convention for it, and that is not a failure: nothing went wrong. The exit code table in the README says so; it is deliberately not one of the codes the CLI assigns itself, all of which still mean what they meant.

## [0.4.9] - 2026-07-31

A new command, and the results of testing every other one before this CLI is announced publicly.

**If a CI step runs `knaix selftest --quick`, read the first entry under Changed.** It no longer fails, and a gate that stops failing does not tell you it has stopped.

### Added

- **`knaix top`.** One live view of every node, hosted and local. Between `knaix metrics`, which is one node and one snapshot, and the dashboard, which is a browser, there was nothing that answered "what is my mesh doing right now" in a terminal. This is that: every node with its status, load and peers, refreshed on an interval, with the selected node's logs streaming underneath. `--interval` sets the refresh, `--lines` the size of the log pane, and `-n` picks the node selected when it opens.

  It is a data layer with a view attached rather than a screen that fetches, so `-o json` emits one snapshot and exits without a terminal being involved at all, which is what makes it usable from a script. CPU, memory and document counts are read less often than the refresh, because `docker stats` samples twice before it can report a rate; a column shows `-` where nothing could be sampled rather than `0%`, which would draw an idle node where there is an unmeasurable one.

- **`knaix local up --generation-timeout <SECONDS>`.** The node gives a model 60 seconds to answer, which a large or reasoning model on consumer hardware passes routinely, and there was no supported way to change it. Bringing your own model is a headline feature and this was the setting it needed. Remembered like the model itself, so a later `up` does not put the timeouts back.

### Changed

- **`knaix selftest --quick` no longer returns a pass or a fail.** It asks 12 of the 52 questions and takes the first two per document rather than sampling, and at that size the interval around a 90% rate is wider than the gap between passing and failing. It was reporting green on a node the full run reported red. It now prints the numbers, says it did not score the node, and exits 0 unless the node failed to answer at all.

  This changes an exit code, which this CLI treats as an interface. A script gating on `knaix selftest --quick` will stop failing rather than start erroring, so it needs changing to a full run to keep its gate.

- **Self-test floors are separated for local and hosted nodes, and only the citation floor differs.** A node answering from a model you brought and run yourself cites more loosely than a hosted frontier model, so holding it to the same citation bar reports the model's size as a defect in the node. Retrieval floors are unchanged and shared, because retrieval does not depend on which model answers: hit rate and MRR are measured over every passage the node returned, which is the output of its own embedder and reranker. Relaxing those would have hidden a reranker that orders badly, which is the one thing MRR exists to report. The report now names which floors were applied.

- **A self-test survives a model that cannot answer in time.** One slow generation used to end the whole run and discard every question already asked. Questions that go unanswered are now recorded with the reason and the run continues. They stay in the denominator, because a node that cannot answer has not retrieved anything either and scoring only what survived would let a node time out its way to a pass, and a run with any of them can never report green.

- **`knaix list local` says why instead of failing to reach the control plane.** It reached for a session and the control plane before noticing the node was local, so listing a local node's documents reported a DNS failure on a machine that was working perfectly. The local node keeps chunks and no document registry, so there is nothing to enumerate, and it now says that and points at the commands that do reach the corpus. `list` also takes `-n`, which every other node command already took.

- **`knaix login` stops before opening a browser when there is nowhere to sign in.** It used to open a browser, wait five minutes, and then report that no sign-in had been completed, which described the user rather than the problem. It now checks first and fails in about a second with the same code every other command gives. Reachability, not health: a control plane answering 403 or 503 on that path can still complete a sign-in, so only a genuine transport failure stops it.

- **`knaix local up` tells a machine that has never installed Docker something it can act on.** Installed-but-stopped and never-installed were the same sentence, and it was "start Docker and try again", which describes a state the second machine was never in. They are now different, and only one of them carries an install link.

### Fixed

- **`knaix memory --file` read files that were not notes.** `--file /etc/passwd` printed it. Joining a path onto a directory replaces the directory outright when the path is absolute, so the notes directory was advisory rather than a boundary. The flag now takes a file name and refuses a path, and because a name that cannot escape can still point somewhere that does, a symlink out of the notes directory is refused on read and is no longer listed as a note.

- **Two piped uploads in one process could share a directory.** The staging directory was named from the process id and the clock, and two calls can read the same instant; the second then wrote into the first's directory and removed it on the way out.

- **A self-test that retrieved nothing reported its MRR as `-0.000`,** and a run where nothing answered claimed the deterministic mock had written the answers when the model you configured had simply never been asked.

## [0.4.8] - 2026-07-30

A security release. **If you have run `knaix local up` on a network you do not control, upgrade.**

### Fixed

- **The local node was published on every network interface.** `knaix local up` disabled the node's authentication on the premise that only this machine could reach it, and then published it on `0.0.0.0`, where anyone on the same network segment could. The premise was right and the port mapping never enforced it. On a shared network, an unauthenticated caller could read, change and delete the knowledge base, read chat history, spend the model, and replace the set of API keys the node accepts, which turns a disclosure into an escalation.

  The node is now published on `127.0.0.1`. A node still running from an older version is re-created on loopback the next time you run `knaix local up`; the store is a named volume and the node keeps its identity, so documents already ingested stay reachable. Nodes reached over a tailnet or provisioned through the control plane were never affected, because they authenticate rather than rely on where the caller is.

- **`knaix local up` no longer touches a container it did not start.** It has always refused a running `knaix-local` that was not its own, but the check rested on the state file, which a plain `knaix local down` deliberately keeps. A container that took the name afterwards inherited the claim. Ownership is now settled by the image, so a container that is not the node is refused rather than adopted or removed, and `up` no longer reports a node running at an address where nothing of yours is listening.

## [0.4.7] - 2026-07-30

### Added

- **`knaix report`.** Writes a diagnostic file you can read, then attach to an issue. The bug template used to ask a reporter to look up their version and their OS, and then ask them not to paste tokens or private hostnames. The first half is work the CLI can do. The second half is a request for care aimed at the person least able to give it, because the output that shows the problem is usually the output with their node's address in it. So the CLI writes the file: the version and how it was installed, the OS, shell and terminal, every check `knaix doctor` runs, recent failures, and recent local node logs.

  What goes in is decided by a list of things known to be safe rather than by looking for things that seem private, which is the difference between a redactor that holds and one that holds until the first input nobody pictured. The token is never included, only its length. Usernames and node names appear as short hashes, stable across runs so two reports from one person still correlate. Log lines are cut back to their timestamp, level, method and route, matched against the routes we actually serve rather than against anything route-shaped. The command then prints what it left out and why, so the redaction can be checked rather than trusted, and the file is yours to read before you send it.

  **The report is never uploaded.** Building one runs the same checks as `doctor`, so it does contact your node and control plane to ask how they are, but what it finds is only ever written to the file. `--open` starts a new issue with your version and OS filled in; you attach the file yourself.

- **Failures and crashes are recorded, so a report run afterwards has something to show.** A CLI that only tells you what went wrong while the terminal is still open is no help the next morning. Failures now append to a rolling file of the last twenty, and there is a panic hook, so a crash is recoverable instead of printing Rust's default and vanishing. Entries are redacted as they are written, not when a report is built, so nothing sensitive is on disk even if you never run `knaix report`. `knaix report --forget` deletes them.

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
