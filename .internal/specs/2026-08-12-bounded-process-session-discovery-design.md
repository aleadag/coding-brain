# Bounded Process Session Discovery Design

**Bead:** `codexctl-5n458.8`

## Context

`cbrain doctor` reports `Codex: 0, Claude: 0, Antigravity: 0` on a host with many live `.codex-wrapped` processes. The process scanner invokes one portable `ps` snapshot with full argv for every process. It runs that command through the terminal capture helper, whose stdout and stderr streams are each limited to 64 KiB.

On the affected host, the exact `ps` columns requested by the scanner produce 191,406 bytes because managed Codex sandbox commands have large argument vectors. The bounded helper rejects the output, `capture_process_snapshot` returns an unsuccessful empty snapshot, and Doctor currently renders that failure as if a successful scan found no sessions.

Transcript files are not live-session authority. The fix must continue requiring recognized live provider process evidence for process-backed Codex, Claude, and Antigravity sessions.

## Requirements

- Discover the current 191 KiB process table without weakening provider recognition or transcript assignment rules.
- Keep process discovery bounded by time and output size.
- Preserve the existing portable `ps` columns and the `comm` plus first-argv executable fallback.
- Preserve environment clearing, process-group termination, and child reaping.
- Keep terminal capture at its existing 64 KiB limit.
- Distinguish an unavailable process scan from a successful scan containing zero sessions.
- Treat unavailable session discovery as a non-fatal Doctor advisory.
- Never expose raw process arguments or unbounded command errors in Doctor output.
- Do not change navigation authority, session identity, provider hook behavior, or transcript-only discovery semantics.

## Considered Approaches

### 1. Process-specific bounded capture — selected

Parameterize the existing bounded command runner with an explicit output limit. Existing terminal callers retain 64 KiB; process discovery uses 1 MiB with the existing 500 ms timeout. Carry process-snapshot availability alongside discovered sessions for Doctor.

This is the smallest change that preserves all current cross-platform recognition behavior. A future table above 1 MiB still fails closed, but Doctor reports discovery as unavailable instead of reporting false zero counts.

### 2. Two-pass `ps`

Collect compact metadata first, then query argv only for recognized PIDs. This lowers ordinary output but risks dropping the current argv fallback when `comm` does not identify a wrapper. Preserving that compatibility would require querying argv for otherwise-unrecognized processes, undermining the intended reduction.

### 3. Streaming provider filter

Parse and discard unrelated `ps` rows while the child runs. This scales best, but introduces substantially more partial-line, oversized-row, cumulative-budget, timeout, and cleanup logic than the current bug requires.

## Architecture

### Bounded command capture

`crates/coding-brain-core/src/terminals/mod.rs` will add an internal `run_bounded_with(command, timeout, max_capture_bytes)` entry point. The existing `run_bounded` function remains the compatibility wrapper for terminal backends and continues to use 500 ms and 64 KiB.

The parameterized runner must apply the supplied limit independently to stdout and stderr, retain nonblocking reads, and preserve the current process-group termination and synchronous reaping behavior.

The 1 MiB per-stream process limit is an explicit safety policy rather than a guarantee that every possible host process table will fit. It permits roughly 2 MiB of captured stdout and stderr before parsing overhead. The 500 ms deadline remains unchanged so the fix does not weaken both memory and latency bounds at once.

### Process snapshot

`crates/coding-brain-core/src/process.rs` will define a 1 MiB process-snapshot stream limit and a 500 ms process-snapshot deadline, then call `run_bounded_with` using those values. The `ps` column list, UTF-8 validation, parsing, recognized executable set, cwd lookup, and start-identity logic remain unchanged.

An output overflow, timeout, spawn failure, nonzero exit, invalid UTF-8 response, or row parse failure continues to produce `ProcessSnapshot { succeeded: false, entries: [] }`. A successful scan with no matching provider processes remains `succeeded: true` with an empty entry list.

### Discovery status

`crates/coding-brain-core/src/discovery.rs` will add `ProviderSessionScan`, a status-aware provider scan result containing:

- the sorted `Vec<AgentSession>` produced by the existing discovery flow; and
- whether the shared process snapshot succeeded.

One availability boolean is sufficient for the public result because timeout, overflow, spawn failure, deliberate discovery disablement, and malformed output all require the same Doctor interpretation. Detailed failure causes remain internal.

The new public `scan_agent_sessions_with_status` function will perform exactly one process snapshot and one Claude inventory refresh. The existing `scan_agent_sessions_with_state` API will delegate to it and return only its sessions so runtime, recovery, and navigation callers retain their current interface and behavior.

### Doctor rendering

`src/doctor.rs` will use the status-aware scan result.

- If the process snapshot failed, Doctor returns an advisory named `session discovery` with a bounded generic message that discovery is unavailable and provider counts may be incomplete. It does not print zero counts as authoritative and does not include raw command errors.
- If the snapshot succeeded and the session list is empty, Doctor retains the existing advisory and suggestion to start a selected provider session.
- If the snapshot succeeded and sessions are present, Doctor retains the pass status and provider counts.

Structured Claude inventory may still yield sessions when the process snapshot fails. Doctor nevertheless reports the scan as unavailable because process-backed counts are incomplete. Any surviving provider counts may appear only when explicitly labeled partial; they must not use the normal complete-count rendering.

## Data Flow

1. Discovery launches the portable `ps` command with an empty environment.
2. The parameterized bounded runner collects each stream for at most 500 ms and at most 1 MiB.
3. A successful UTF-8 response is parsed into recognized provider process entries.
4. The existing Codex transcript assignment, Claude structured-inventory merge, and Antigravity process projection produce sessions.
5. Discovery returns sessions plus process-snapshot availability.
6. Doctor selects unavailable, successful-empty, or successful-nonempty rendering without inspecting or exposing raw argv.

## Error Handling and Security

- The new limit is finite and local to process discovery; terminal capture stays at 64 KiB.
- Both stdout and stderr remain bounded.
- The existing deadline prevents an indefinitely producing or stalled `ps` child.
- The existing Unix process-group kill and reap behavior remains authoritative for cleanup.
- The command environment remains cleared.
- Malformed or oversized output fails the complete snapshot rather than producing partially trusted process identity.
- Doctor exposes a fixed advisory category, not raw process rows, argv, paths, or command error strings.
- Raw process command lines must also remain absent from internal errors that Doctor could render.
- A failed snapshot must never be rendered as a successful count of zero.

## Testing

### Bounded runner

- Retain the existing test proving default `run_bounded` rejects output above 64 KiB.
- Add a parameterized-runner test proving a larger explicit limit accepts output above 64 KiB.
- Add a parameterized-runner test proving output above its explicit limit is rejected.
- Generate large fixtures with portable Rust test helpers rather than shell-specific loops or oversized command-line arguments.
- Retain timeout, inherited-pipe, descendant-cleanup, nonzero-exit, and process-group cleanup coverage.

### Process snapshot

- Replace the old expectation that 70,000 bytes necessarily makes process discovery unavailable.
- Add a realistic `ps` fixture above 64 KiB and below 1 MiB with recognized rows on both sides of the old 64 KiB boundary, proving the entire successful snapshot is parsed.
- Add output above 1 MiB and prove the snapshot is unsuccessful and empty.
- Retain successful-empty versus malformed-output coverage.

### Discovery and Doctor

- Prove the status-aware scan reports `process_snapshot_succeeded = false` independently of any structured Claude sessions.
- Prove Doctor renders failed process discovery as an unavailable advisory without authoritative zero counts.
- Prove a successful empty scan retains the existing zero-session advisory.
- Prove a successful nonempty scan passes with stable provider counts.

### Verification

- Run focused bounded-runner, process-snapshot, provider-discovery, and Doctor tests.
- Run the serial workspace test suite.
- Run formatting, Clippy with warnings denied, and the workspace build.
- Run relevant Nix formatting and packaged checks.
- Run a locally built `cbrain doctor --json` outside a PID sandbox and confirm it discovers currently running Codex sessions on the host whose `ps` output is 191 KiB.

## Documentation

- Add an `[Unreleased]` changelog entry for bounded process discovery and truthful unavailable reporting.
- Update the reference and troubleshooting text to distinguish a successful zero-session scan from unavailable discovery.

## Non-Goals

- Streaming or two-pass process enumeration.
- Persisting or restoring stale process snapshots across failures.
- Changing provider executable matchers or transcript assignment heuristics.
- Adding transcript-only sessions without live process authority.
- Changing Doctor severity beyond the selected advisory behavior.

## Stress Test Results

The approved design was challenged across eight branches:

1. Keep 1 MiB per stream as a deliberate safety ceiling; overflow is unavailable/incomplete, never authoritative zero.
2. Keep the 500 ms deadline and surface timeout truthfully rather than increasing both resource bounds.
3. Carry one availability boolean with sessions; keep individual failure causes internal.
4. Label surviving structured-provider results partial whenever the shared process snapshot fails.
5. Preserve the existing runtime list API; last-good snapshot persistence and stale-state lifecycle handling remain out of scope.
6. Accept at most 1 MiB each for stdout and stderr while preserving kill/reap cleanup and excluding raw argv from diagnostics.
7. Use portable Rust-generated fixtures to cover the old boundary, the new ceiling, truthful failures, and unchanged terminal defaults.
8. Limit documentation changes to the changelog, command reference, and troubleshooting guide.

The final reflexion pass found no additional unresolved branch. Deliberate discovery disablement, malformed output, process-launch failures, Claude-only partial results, and runtime compatibility are covered by the decisions above.
