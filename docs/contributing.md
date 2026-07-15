# Contributing

codexctl is a three-crate Rust workspace with one dependency direction:

```text
codexctl -> codexctl-tui -> codexctl-core
```

`codexctl-core` owns session types, transcript discovery, health checks, terminal backends, and runtime contracts. `codexctl-tui` owns the dashboard, local skill view, demo fixtures, and recording. The root package wires those crates to the local brain, configuration, onboarding, and CLI.

```text
crates/
├── codexctl-core/
└── codexctl-tui/
src/
├── brain/
├── runtime/
├── config.rs
├── doctor.rs
├── init/
└── main.rs
```

The runtime composition exposes sessions, brain views, immediate actions, review data, and brain mailbox delivery. Keep binary-only brain code out of `codexctl-core`.

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Add tests for status inference, health checks, terminal backends, configuration parsing, and brain behavior when changing those areas. Keep changes surgical and preserve the existing config and state paths unless a task explicitly includes migration.

## Product boundary

New work should strengthen the live session dashboard, local brain, deterministic rules, learning/review loop, mailbox delivery, or terminal integration. Durable queues, dependency scheduling, distributed transport, and persistent worker coordination belong in external tools.

Beads is the repository's task tracker and can also be recommended to users who need durable coordination, but codexctl must not require it at runtime.
