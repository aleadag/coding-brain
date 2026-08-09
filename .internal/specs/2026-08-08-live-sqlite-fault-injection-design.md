# Live SQLite fault injection

- Date: 2026-08-08
- Task: `codexctl-dzlb9.11`
- Brainstorming: `codexctl-dzlb9.11.1`
- Status: Approved and stress-tested

## Summary

Task 11 will test each of its eight fault points through real `cbrain`
processes. A non-default Cargo feature named `fault-injection` will compile the
test controls into an otherwise normal binary in any build profile. Without
that feature, the control types, CLI arguments, dispatch, and environment reads
do not exist.

Permission and delivery faults run through the real provider hook path.
Checkpoint and migration faults run through their owning non-hook paths, after
which a Codex, Claude, or Antigravity hook opens the resulting state and proves
the provider-visible restart behavior. This gives every provider/fault pair
process-level evidence without allowing a hook to perform maintenance or
migration.

Feature-enabled fault controls also require an explicit command-line selection
and a capability tied to an isolated state root. The capability prevents an
accidental invocation from targeting ordinary user state. It is an accident
barrier, not a security boundary against the user who deliberately built the
binary with `--features fault-injection`.

## Problem

The Task 11 brief requires a three-provider by eight-fault live-process matrix:

```rust
enum FaultPoint {
    AdmissionWrite,
    InferenceExit,
    CommitBeforeCall,
    CommitAfterReturn,
    StdoutWrite,
    DeliveryWrite,
    Checkpoint,
    MigrationPublish,
}
```

Current deterministic SQLite injection is available only to unit tests.
Permission stage injection is also compiled only under `cfg(test)`, while the
migration abort seam is controlled by `debug_assertions` and an environment
variable. Cargo integration tests execute `CARGO_BIN_EXE_cbrain`, where the
unit-test-only seams are absent. The existing implementation can therefore
cross all providers with inference-exit and stdout failures, but it cannot run
the remaining six points through live binaries.

Making the current environment controls unconditional would expose accidental
fault activation in shipped hooks. Making hooks invoke checkpoint or migration
would violate the storage role boundary: hooks must remain bounded and must
never perform maintenance or migration. The matrix needs a live test boundary
that preserves both constraints.

## Goals

- Run all eight Task 11 fault points through real processes.
- Cross every fault with Codex, Claude, and Antigravity behavior.
- Assert exact persisted state, stdout, fallback, and restart behavior.
- Keep fault activation absent from ordinary debug and release binaries.
- Preserve the prohibition on hook-driven maintenance and migration.
- Replace profile-dependent and ambient-environment activation.

## Compile-time boundary

The root package adds an empty, non-default Cargo feature:

```toml
[features]
default = []
fault-injection = []
```

The fault-point enum, activation parser, process-local controller, hidden CLI
arguments, and live fault branches are all guarded by
`cfg(feature = "fault-injection")`. The feature is independent of
`debug_assertions`: CI may exercise the matrix with either a debug or release
profile, and no correctness claim depends on the profile.

Normal `cargo build`, `cargo build --release`, Nix builds, musl builds, and
crate packaging do not enable the feature. Their `cbrain` binaries reject the
fault-control arguments as unknown. CI tests that behavioral absence and checks
the release workflow for an accidental `--features fault-injection` addition.
Feature-enabled checks and official release builds use separate Cargo target
directories; linkage inspection and artifact upload consume only the clean
default-feature release directory.

A developer who deliberately compiles the source with the feature receives the
controls. Cargo features cannot prevent that, so the feature is not described
as an authorization boundary or as safe for ordinary operation.

## Activation and isolated-state capability

A feature-enabled process accepts hidden internal arguments selecting exactly
one fault point and naming a capability file. Ambient environment variables and
hook JSON cannot select or configure a fault.

The integration harness creates the capability outside the Coding Brain state
directory so migration and database-directory validation never mistake it for
managed storage. The bounded, versioned file contains:

- the canonical isolated Coding Brain state-root path;
- a fresh random nonce; and
- exactly one tagged selection: either a Task 11 matrix fault point or a value
  from the closed internal migration-regression stage enum; and
- the intended control pipe's device and inode.

The stage detail preserves the current deterministic migration crash suite when
its debug-environment seam is removed without labeling build, review, or freeze
stages as `MigrationPublish`. It is never a free-form stage string; the 24-cell
provider matrix uses the matrix `MigrationPublish` selection only at
`AfterBrainPublication`.

The process receives the same nonce explicitly. Before storage opens, it opens
the capability once with no-follow semantics, validates trusted ancestors with
the existing secure-path rules, and checks owner, mode, regular-file type, and
link count from the open descriptor. It reads the bounded contents from that
same descriptor and keeps it open until controller initialization completes.
The recorded state root must exactly match the process's resolved XDG Coding
Brain state root. A mismatch, malformed capability, unsafe topology, or
unsupported fault exits before touching storage.

The feature gate is the compile-time separation. The capability adds protection
against a test command accidentally inheriting a developer's real XDG paths;
it is not intended to resist another process running as the same user.

The process-local controller is initialized once from the validated CLI
arguments. Each configured fault can fire only once. An inherited control pipe
carries one small fixed marker identifying the point and its before/after
position, plus the closed migration-stage detail when applicable, when the seam
is reached. The marker is test control data and never enters provider stdout,
stderr, payloads, or SQLite. A missing, duplicate, or wrong marker fails the
case, so an unrelated crash or bypassed seam cannot silently turn a matrix row
green.

The parent clears close-on-exec only for the initial `cbrain` spawn. The child
rejects descriptors below 3, requires a write-only FIFO, and compares its device
and inode with the capability before restoring `FD_CLOEXEC`, so stdout or
another inherited pipe cannot substitute for the marker channel. Markers fit
within `PIPE_BUF`; the parent uses a bounded read deadline, reaps the hook
process, and rejects extra bytes, timeouts, or a pipe that remains open after
the hook exits.

## Fault semantics

The eight public matrix names remain stable even when the owning low-level seam
has a more specific name.

| Fault point | Owning process and injected event | Required durable result |
| --- | --- | --- |
| `AdmissionWrite` | Provider hook returns a deterministic SQLite write failure inside the admission transaction. | No partially admitted request or executable output; provider-native fallback remains available. |
| `InferenceExit` | The configured inference child exits before returning a decision. | Observed and evaluating activity terminate in the existing bounded error state, with no proposal and no replay. |
| `CommitBeforeCall` | Provider hook terminates immediately before calling `transaction.commit()`. | Admission rows may remain, but no proposal, terminal authority, or provider response becomes durable. |
| `CommitAfterReturn` | Provider hook terminates immediately after `transaction.commit()` returns under enforced `synchronous = FULL` and before stdout. | The exact proposal and terminal authority exist once; delivery remains pending or unknown and restart never emits the response. |
| `StdoutWrite` | The parent closes the hook's stdout pipe before the provider response write. | The committed decision remains singular and delivery becomes failed; restart does not replay it. |
| `DeliveryWrite` | After stdout succeeds, the provider hook receives a deterministic SQLite failure while committing delivery evidence. | Captured stdout is exact; durable delivery remains pending or unknown rather than falsely delivered, and restart does not replay. |
| `Checkpoint` | A non-hook worker faults the real maintenance checkpoint call. | Previously committed WAL rows remain readable and maintenance reports the exact checkpoint fault category. A provider hook then opens the same store without running maintenance. |
| `MigrationPublish` | A non-hook worker terminates at the existing publication boundary after the migrated artifact is durable. | Hook preflight preserves native fallback while migration is incomplete. A non-hook restart resumes to one complete generation, after which the provider hook observes the canonical store without duplicate rows. |

SQLite failures use the existing `StorageOperation` and extended-result-code
mapping. The commit points deliberately bracket the public transaction commit
call; they do not claim to intercept SQLite's internal `fsync`. Existing unit
coverage remains responsible for ambiguous `SQLITE_IOERR_FSYNC` mapping.
Process-termination tests require the control marker and an abnormal exit but
do not require one numeric status or signal across Linux and macOS. The test
does not substitute a mock store, rewrite SQLite files, or infer success from a
process exit alone.

## Process orchestration

The matrix is parameterized by `AgentProvider` and `FaultPoint`, producing 24
named cases. Each case receives a fresh temporary config root, state root,
capability, database, provider identity, session, turn, tool-use identity, and
captured stdout pipe.

Six points execute the normal hidden permission-hook command with the selected
provider. `StdoutWrite` is induced by the parent process closing the pipe;
`InferenceExit` uses the real inference-process boundary. The remaining hook
points are armed through the feature-gated controller and reached by normal
permission storage and delivery code.

`Checkpoint` and `MigrationPublish` use composite restart scenarios with two
process roles:

1. a feature-gated non-hook worker invokes the owning maintenance or migration
   operation and faults at the selected boundary;
2. a normal provider-hook invocation, built with the same feature but with no
   fault armed, opens the resulting state and supplies the provider-specific
   observation;
3. where required, a second non-hook worker completes recovery; and
4. another unarmed provider hook proves the stable post-restart state.

The worker is internal test dispatch, not a production storage abstraction.
It calls the same `MigrationCoordinator` and maintenance APIs used by normal
non-hook startup. `OpenRole::Hook` continues to reject those operations, with a
regression test proving the feature does not bypass that check.

For these six cells, the provider dimension describes the hook that observes
and recovers from the faulted state; it does not imply that checkpoint or
migration behavior varies by provider.

## Exact assertions

Every matrix cell queries SQLite by provider, session, turn, tool-use,
permission-attempt, activity, and decision identity. Tests compare ordered row
sequences and exact counts; provider-wide totals are not sufficient.

The assertions cover:

- exact stdout bytes: empty native fallback for Codex and Claude, Antigravity's
  exact native `ask` response where fallback applies, and exact provider allow
  or deny bytes where output precedes the fault;
- exact activity states and ordering;
- exact proposal and permission-commit counts;
- lifecycle authority and delivery state;
- migration generation and publication state;
- checkpoint outcome and preservation of committed WAL rows; and
- a second unarmed invocation that produces no replay or duplicate mutation.

Fixtures fix the expected rows for each fault instead of deriving expectations
from the implementation under test. Each test also confirms through the
dedicated control pipe that its configured fault fired exactly once.
`StdoutWrite` additionally requires the exact durable `DeliveryFailed` row
produced by the closed pipe.

## Failure behavior

Invalid or missing activation fails before opening SQLite. An unknown point,
unsafe capability, state-root disagreement, or unreachable fault cannot fall
back to an unfaulted successful run.

Fault injection does not change production error mapping or thresholds. It
enters at existing call-site boundaries and returns the same SQLite error or
process interruption that the owning code already handles. Any matrix failure
is fixed in the owning storage or provider module; tests must not raise
deadlines, weaken filesystem checks, enable hook maintenance, or relax exact
row assertions.

The existing debug-build reads of
`CODING_BRAIN_SQLITE_MIGRATION_FAULT` are removed. Migration crash tests move to
the feature-gated controller, and a default-binary regression proves that the
old environment variable no longer activates a fault. Unit-only permission
injection may remain under `cfg(test)` because it cannot enter an integration
test's `CARGO_BIN_EXE_cbrain`.

## Verification and release gates

Focused verification adds a dedicated feature-enabled invocation for the live
matrix. The ordinary Task 11 command remains feature-free and must continue to
pass, proving the controls are not required for normal behavior.

Release verification covers:

- default debug and release binaries, built in a clean target directory, reject
  the internal arguments;
- a feature-enabled release-profile binary, built in a separate target
  directory, recognizes the arguments but rejects an invalid capability;
- the normal release workflow, Nix expression, musl commands, and package
  commands never enable `fault-injection`;
- the existing Linux and macOS checks still prove no dynamic `libsqlite3`;
- a feature-enabled release-profile matrix can be run separately to prove the
  seam does not depend on debug assertions; and
- `cargo package --workspace --allow-dirty` succeeds with default features.

The feature-enabled binary is a test artifact. It is never installed, uploaded,
or substituted for the artifact inspected by release linkage checks.
`flake.nix` retains `checkType = "debug"`: that setting selects the stock Nix
package-check profile and does not activate the feature. Linux and macOS run
the live feature matrix; musl targets compile the feature while retaining their
existing runtime-test scope unless executable runners are available.
Linux and macOS regressions also prove that inference children do not inherit
the fault-control descriptor.

## Rollback

Fault injection changes no schema or persistent data contract. Capabilities
live outside managed state and the parent harness removes their temporary
directories even when a child crashes. Official artifacts are already
feature-free, so operational rollback requires no action. Development rollback
reverts the controller, CLI, converted tests, CI commands, and removal of the
old debug-only migration seam together; partially removing the feature while
leaving crash coverage disabled is not valid rollback. Stores left by fault
tests are ordinary SQLite stores and remain readable by the default binary.

## Non-goals

- Exposing a supported user-facing fault-injection CLI.
- Treating the capability file as protection against a malicious same-user
  process.
- Letting provider hooks run migration, checkpoint, or retention maintenance.
- Adding a general SQLite VFS, arbitrary SQL failure scripting, or multi-fault
  scenario language.
- Changing storage deadlines, retry budgets, schemas, provider protocols, or
  native fallback policy.
- Replacing the existing unit fault tests, contention tests, source-race tests,
  or large-store coverage.

## Acceptance criteria

- The live matrix contains exactly 24 provider/fault cases over the Task 11
  enum.
- Each case proves exact stdout, persisted identity-qualified rows, restart
  state, and zero replay.
- Normal debug and release binaries contain no recognized fault-control CLI.
- Feature-enabled release-profile tests work without `debug_assertions`.
- Hook payloads and ambient environment variables cannot activate a fault.
- Capability validation prevents accidental use against a different or unsafe
  state root.
- Hooks remain unable to perform checkpoint, maintenance, or migration.
- No production threshold, storage invariant, or provider fallback contract is
  weakened.

## Stress Test Results: Live SQLite fault injection

### Resolved Decisions

- The non-default Cargo feature is an explicit custom-build surface, not an
  authorization boundary. Official artifacts omit it and isolated-state
  capability validation prevents accidental targeting.
- Capability validation uses one no-follow descriptor and existing secure-path
  checks, avoiding a metadata/open race.
- Linux and macOS require a control marker plus abnormal exit without assuming
  one signal encoding; musl targets compile the feature.
- The misleading commit names changed to `CommitBeforeCall` and
  `CommitAfterReturn`. They bracket SQLite's `FULL`-synchronous commit API while
  unit tests retain `IOERR_FSYNC` uncertainty coverage.
- Checkpoint and migration rows are explicitly composite scenarios: non-hook
  workers own the fault, and provider hooks own post-fault observation.
- A dedicated inherited write-only FIFO proves that each armed seam fired
  exactly once and distinguishes the intended fault from an unrelated crash.
  Its descriptor must be at least 3 and its `(device, inode)` must match the
  capability record before close-on-exec is restored.
- `cbrain` restores close-on-exec before spawning inference, while bounded
  parent reads and exact marker framing prevent descriptor leaks from hanging or
  falsely satisfying a case.
- Feature and official release artifacts use separate target directories;
  linkage inspection and uploads consume only default-feature output.
- Debug-environment migration activation is replaced by a tagged closed
  selection: matrix faults and the existing migration-regression stages cannot
  be confused or extended with free-form strings. Nix keeps its unrelated
  debug check profile.

### Changes Made

- Renamed the two commit fault points and clarified their durability claims.
- Strengthened capability validation, process observability, platform behavior,
  artifact separation, composite-role reporting, and rollback requirements.
- Added removal and regression coverage for the old debug migration environment
  seam.
- Made development rollback atomic so removal cannot silently discard the
  migrated crash-stage coverage; operational rollback remains a no-op.

### Deferred / Parking Lot

- Musl executes the live matrix only when CI provides target runners; feature
  compilation remains mandatory on both existing musl targets.
- The feature remains visible to deliberate source builders because Cargo has
  no private-feature boundary. It stays internal and unsupported.

### Confidence Assessment

- Overall: High.
- Areas of concern: inherited descriptor setup differs between Linux and macOS,
  so the plan must include focused capability/control-pipe and close-on-exec
  tests on both CI runners. The live crash points bracket SQLite's API rather
  than internal filesystem syscalls, which is now explicit and covered
  separately at the error-mapping layer.
