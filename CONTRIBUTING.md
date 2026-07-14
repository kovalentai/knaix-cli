# Contributing to Knaix CLI

Thanks for your interest in Knaix. This document covers how to build,
test, and propose changes.

## Reporting security issues

**Do not open a public issue or pull request for a security problem.**
Follow the private process in [SECURITY.md](SECURITY.md) instead.

## Development setup

Knaix is a Rust project. You need a stable Rust toolchain
(`rustup` recommended).

```bash
git clone https://github.com/kovalentai/knaix-cli.git
cd knaix-cli
cargo build
```

## Before you open a pull request

CI runs the following, and they must pass. Run them locally first:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
cargo test --locked
```

New behavior should come with tests. Security-sensitive code paths
(authentication, config, filesystem permissions) should keep or extend
their regression coverage.

## Never commit secrets

This is a public repository. Do not commit tokens, API keys, `.env`
files, real hostnames from private infrastructure, or the local state
files Knaix writes at runtime (`~/.knaix/`, `_knaix_durable_memory.md`,
`_knaix_ephemeral_log.md`). If you believe a secret was committed, treat
it as compromised: rotate it and report privately per SECURITY.md.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/)
(`feat:`, `fix:`, `docs:`, `test:`, `chore:`). Keep messages factual and
free of sensitive detail. Describe the change, not exploit specifics.

## Pull request checklist

- [ ] `cargo fmt`, `clippy`, `build`, and `test` all pass locally.
- [ ] Tests added or updated for the change.
- [ ] No secrets, tokens, or private hostnames in the diff or history.
- [ ] Docs updated if behavior or flags changed.
