# Just Run Recipe Design

## Context

The `justfile` exposes build and quality-gate recipes but lacks a general way
to launch codexctl. Its `headless-json` recipe is a narrow alias for arguments
the CLI already accepts.

## Decision

Add a zero-or-more variadic `run` recipe that invokes `cargo run --` and
forwards every supplied argument. Remove `headless-json`; its behavior remains
available as `just run --headless --json`.

## Verification

- `just --list` shows `run` and does not show `headless-json`.
- `just --dry-run run` renders `cargo run --`.
- `just --dry-run run --headless --json` forwards both flags.
