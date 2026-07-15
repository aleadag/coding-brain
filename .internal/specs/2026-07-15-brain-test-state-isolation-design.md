# Brain Test State Isolation Design

**Date:** 2026-07-15  
**Status:** Approved for implementation

## Context

`BrainEngine` unit tests exercise real decision logging. Decision persistence currently derives its directory from `HOME`, so ordinary `cargo test` runs append fixture records such as `project: "test"` to the user's live `~/.codexctl/brain/decisions.jsonl`. Those records are then rebuilt into the Brain Review queue as high-confidence misses.

The live store cleanup is separate from the code fix: preserve the original JSONL file, remove only records whose exact `project` field is `"test"`, and verify that the rendered review queue is empty.

## Decision

Keep the production brain data path unchanged. In unit-test builds only, `decisions_dir()` will return a directory below the operating system temporary directory, namespaced by the test process and current test thread. All brain modules already derive their persisted paths from `decisions_dir()`, so this isolates engine decision logs without adding a production option or changing call sites.

The test path must not depend on process-wide `HOME`. Thread identity prevents parallel unit tests from sharing brain state; process identity prevents the separate Cargo test binaries from sharing it.

## Alternatives

- Inject a persistence trait into `BrainEngine`: architecturally flexible, but broad for a test-only leak.
- Mutate `HOME` in engine tests: smaller, but process-global environment mutation is unsafe under Cargo's parallel test runner.
- Disable decision logging under tests: prevents the leak but makes unit-test behavior diverge at the logging boundary.

## Safety and Verification

- Add a regression test that fails while `decisions_dir()` still points beneath `HOME` and passes only when it resolves beneath the test namespace.
- Run the focused regression test, full `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo build`.
- Snapshot the live decision-log counts before and after the full suite; the suite must add zero records to the live file.
- Confirm `codexctl --brain-review list` remains empty after the suite.

## Consequences

Production persistence remains under `~/.codexctl/brain`. Unit-test artifacts become disposable temporary data and cannot teach the live brain or populate its review queue. Test-only temporary directories may remain until normal OS temporary-file cleanup, which is acceptable and avoids global cleanup races.
