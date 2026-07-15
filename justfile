# codexctl developer shortcuts.

# Show available recipes.
default:
    @just --list

# Build the workspace.
build:
    cargo build

# Run the test suite.
test:
    cargo test

# Type-check all targets.
check:
    cargo check

# Format the workspace.
fmt:
    cargo fmt

# Check formatting without modifying files.
fmt-check:
    cargo fmt --check

# Run clippy with warnings denied.
clippy:
    cargo clippy -- -D warnings

# Run headless mode as JSON.
headless-json interval="2000":
    cargo run -- --headless --json --interval "{{interval}}"
