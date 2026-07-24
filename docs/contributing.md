# Contributing

Coding Brain is a three-crate Rust workspace with one dependency direction:

```text
coding-brain -> coding-brain-tui -> coding-brain-core
```

- `coding-brain-core` owns session evidence, transcript discovery, health checks, Coding Brain paths, project identity, terminal backends, and runtime contracts.
- `coding-brain-tui` owns the Live, Review, Scorecard, and Diagnostics application and terminal suspend/restore behavior.
- the root package owns local Brain evaluation, persistence, config parsing, onboarding, hooks, and the `coding-brain` CLI.

Core must not import root-package modules. The TUI communicates with the binary through runtime traits in `coding-brain-core`; optional Agent Deck navigation is represented as a navigation plan rather than a direct dependency on binary internals.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Nix and packaging changes should also pass:

```bash
nix flake check
just check
```

When changing status inference, health checks, activity retention, hooks, paths, or purge behavior, extend the existing focused tests. Purge tests must use injected absolute paths and must prove that symlinks are unlinked rather than followed.

## Product scope

New work should strengthen immediate judgment, the hook-first activity record, preference learning, Live/Review/Scorecard/Diagnostics, or reliable session navigation. Durable queues, dependency scheduling, distributed transport, and persistent worker coordination belong in external tools.

Beads is the repository task tracker and an optional user companion. Agent Deck is an optional navigation companion. Neither belongs in Coding Brain's required runtime dependency graph.
