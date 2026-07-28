# Nix Fake-Curl Fixture Deflake Design

## Context

The Unix tests in `src/brain/client.rs` generate a temporary `curl` shell
script, make it executable, and immediately spawn it. Nix derivation
`k56jdy5k0kaxs2qj6r44mb0pzkvxfa9q-coding-brain-0.58.0.drv` reproduced an
intermittent spawn failure:

```text
curl failed: Text file busy (os error 26)
```

The failure occurs before the script body runs. `std::fs::write` has already
closed its file handle before permissions are changed and the process is
spawned, so adding another explicit close does not change the ordering.

## Decision

Keep production curl invocation unchanged. Change only the test seam and
fixture so tests invoke the generated script through a shell selected from the
test process `PATH`:

```text
sh <temporary-script> <curl arguments...>
```

The temporary script remains a normal non-executable file. The existing
path-only production helper remains in place and delegates to a narrower
executor that accepts a prepared `Command`. Production continues to prepare
`Command::new("curl")`; tests prepare `Command::new("sh").arg(script)`. All
existing curl arguments, piped stdin, bounded stdout/stderr handling, and
response parsing remain in the shared executor.

Each fixture invocation owns a unique `TempDir` and prepared command. The
`TempDir` remains alive through process completion, and tests do not mutate
global environment or serialize parallel execution.

## Alternatives Considered

- Retry `ETXTBSY`: rejected because it adds production behavior for a
  test-fixture race and masks the failing execution mechanism.
- Add sleeps, `sync_all`, or another explicit `drop`: rejected because the
  writer is already closed and timing does not provide a deterministic
  contract.
- Replace fake curl with a compiled helper or local HTTP server: rejected as
  substantially broader than needed for the existing argv/stdin seam.

## Error Handling and Security

Production error messages and fail-safe behavior remain unchanged. Tests still
execute only their own temporary script, without shell interpolation of the
script path or curl arguments. Resolving `sh` through `PATH` matches the pure
Nix test environment and avoids a hard-coded FHS interpreter path.

## Testing

1. Extend an existing argv/stdin test to assert that the generated fixture has
   no execute bits. The existing fake-curl cases prove that the prepared shell
   command executes it and preserves `$0`, `$@`, and stdin behavior.
2. Run the affected `brain::client::tests`.
3. Run `nix build .#` so Nix evaluates and tests the modified source in a new
   derivation, then run `nix build .# --rebuild` to execute its package checks
   again. Retain the old derivation hash only as failure evidence.
4. Run formatting, workspace tests, and Clippy with warnings denied.

## Scope

Only `src/brain/client.rs` and its tests change. No retry, timeout, provider,
payload, parsing, or public API behavior changes.

## Stress Test Results: Nix Fake-Curl Fixture Deflake

### Resolved Decisions

- Preserve the path-only production helper and isolate prepared commands in a
  private executor.
- Resolve `sh` from the test process `PATH`, not `$SHELL` or `/bin/sh`.
- Preserve fixture `$0`, `$@`, and stdin behavior and assert the absence of
  executable bits.
- Retain `TempDir` ownership through execution and avoid retries, sleeps,
  synchronization, global locks, or shared fixture paths.
- Keep script contents as test literals and pass paths and arguments without
  shell-string interpolation.
- Validate the modified source with a newly computed Nix derivation.
- Keep the prepared-command executor private, the shell fixture Unix-test-only,
  and the single-file change independently revertible.

### Changes Made

- Narrowed the prepared-command seam so the existing production wrapper
  remains path-only.
- Corrected Nix verification to build the modified source instead of the
  historical failing derivation.
- Made fixture lifetime, parallelism, and the non-executable regression
  invariant explicit.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: The original kernel/filesystem timing is intermittent, so
  verification proves removal of direct script execution rather than forcing
  the old race to occur.
