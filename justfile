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

# List configured loops.
loop-list:
    cargo run -- loop list

# Validate all configured loops.
loop-validate:
    cargo run -- loop validate

# Validate one configured loop.
loop-validate-one name:
    cargo run -- loop validate "{{name}}"

# Run a loop once.
loop-run name="issue-triage":
    cargo run -- loop run "{{name}}"

# Dry-run a loop without submitting tasks.
loop-dry-run name="issue-triage":
    cargo run -- loop run "{{name}}" --dry-run

# Show loop item status.
loop-status name="issue-triage":
    cargo run -- loop status "{{name}}"

# Show loop item logs.
loop-logs name="issue-triage":
    cargo run -- loop logs "{{name}}"

# Pause a loop.
loop-pause name="issue-triage":
    cargo run -- loop pause "{{name}}"

# Resume a paused loop.
loop-resume name="issue-triage":
    cargo run -- loop resume "{{name}}"

# Show supervisor task status.
supervisor-status:
    cargo run -- supervisor status

# Show supervisor task status filtered by state.
supervisor-status-state state:
    cargo run -- supervisor status --state "{{state}}"

# Show detailed supervisor task logs.
supervisor-logs task:
    cargo run -- supervisor logs "{{task}}"

# Cancel a supervisor task.
supervisor-cancel task:
    cargo run -- supervisor cancel "{{task}}"

# Run headless mode as JSON.
headless-json interval="2000":
    cargo run -- --headless --json --interval "{{interval}}"
