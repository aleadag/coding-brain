# codexctl

Codex-only control plane for supervising, coordinating, and learning from Codex sessions.

## Build And Test

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Run `cargo fmt` and `cargo clippy -- -D warnings` before committing.

## Architecture

This is a three-crate Cargo workspace. Dependencies flow downward:

```text
codexctl -> codexctl-tui -> codexctl-core
```

The workspace crates are `codexctl-core` and `codexctl-tui`; the runtime integration is Codex-only.

```text
crates/
├── codexctl-core/    # session types, Codex transcript discovery, monitor, runtime traits
└── codexctl-tui/     # terminal UI, recording, demo fixtures
src/                   # codexctl binary: brain, bus, coord, hive, relay, init
```

`codexctl-core` must not depend on binary-only modules. The TUI communicates with the binary through runtime traits in `crates/codexctl-core/src/runtime.rs`.

## Codex Integration

- Session discovery reads recursive `~/.codex/sessions/**/rollout-*.jsonl`.
- Hook install writes `.codex/hooks.json` or `~/.codex/hooks.json`.
- Skill discovery scans `~/.codex/skills`, `~/.codex/plugins/*/skills`, and project `.codex/skills`.
- Terminal launch paths invoke `codex` or `codex exec`.

## Conventions

- Keep changes surgical and tied to the request.
- Do not add abstractions for one-off behavior.
- Match existing style unless the migration requires a public rename.
- Config fields must be added to all three layers: CLI args in `main.rs`, TOML structs in `src/config.rs`, and merge logic in `src/config.rs`.
- All jj descriptions created or updated in this repo must use the emoji conventional format: `<emoji> <type>: <imperative summary>`.
- When starting a jj changeset before editing, set the initial description in that format; do not use a plain temporary subject.
- When asked to write, update, or curate a commit message, use the `commit-message` skill if available.
- In jj repos, honor the exact user-provided revset for commit-message work; do not assume `@`.
- For jj commit messages, inspect with `jj --no-pager show --git <revset>` or `jj --no-pager diff --git`, apply with `jj describe -r <revset> -m "<emoji> <type>: <imperative summary>"`, then verify with `jj --no-pager st` and `jj --no-pager log -r '<revset>|@' --no-graph`.
- Status inference logic has extensive coverage; update tests when changing it.
- Health checks in `crates/codexctl-core/src/health.rs` have unit coverage; add tests for new checks.
- Terminal backends implement the pattern in `crates/codexctl-core/src/terminals/mod.rs`.

## Compatibility Notes

Persistent state remains under `~/.codexctl` and config remains under `.codexctl.toml` / `~/.config/codexctl/config.toml` for existing installs. Do not rename those storage paths unless the task explicitly includes a data migration.
