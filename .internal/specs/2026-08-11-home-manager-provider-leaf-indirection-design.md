# Home Manager provider leaf indirection

## Goal

`cbrain doctor` must accept the current Home Manager provider topology, where each global provider path links to the exact location inside one `*-home-manager-files` generation and that generation leaf links once more to an immutable Nix-store source. The outer generation link establishes Home Manager ownership; the inner link is validated only to decide whether Doctor may safely read its content. This read-only exception must not weaken project-scope or imperative mutation protections.

## Root cause

The earlier Home Manager inspection design accepts only a regular final leaf. Current Home Manager generations render the expected Codex, Claude, and Antigravity leaves as absolute links to other `/nix/store` objects. `read_provider_file_for_inspection` rejects those leaves as `home_manager` / `invalid` / `unsupported_topology` before comparing their JSON.

The packaged Doctor fixture copies provider JSON into regular generation leaves, so it proves the earlier shape rather than production. The installed provider commands and running `cbrain` resolve to the same package generation; command staleness is not the cause.

## Design

### Bounded read-only leaf resolution

Keep the existing outer-link contract unchanged:

- the home path must be a global provider path;
- its target must be absolute and lexically clean;
- the target must be under the configured Nix-store root;
- the first relative component must end in `-home-manager-files`;
- the remaining suffix must exactly match the provider path;
- the store root, generation, and suffix parent directories must be real non-symlink directories.

After validating that contract, inspect the expected leaf:

1. A regular file follows the existing bounded read path.
2. At this point the file is Home Manager-owned, matching Home Manager's own collision-checking rule for links into `*-home-manager-files`. This applies to retained older-generation links as well as the active generation; provider semantic comparison still determines whether their commands are current or stale. Do not query the active profile or Nix database to establish ownership.
3. A symlink is accepted only as one additional content-read hop. Its raw target must be an absolute, lexically clean path under the same Nix-store root. It must begin beneath one top-level Nix store object rather than a store-internal namespace; normal suffix components beneath that object are allowed for sources such as Claude's generated settings directory.
4. Every parent component of the inner target must be a real non-symlink directory. The inner target must be a non-symlink regular file.
5. Open the captured inner target, confirm the opened handle is a regular file, and retain the existing 1 MiB bound before comparing provider JSON.

Do not canonicalize either link. Canonicalization would hide intermediate topology and could turn an unsupported chain into an apparently valid final path.

Relative, non-store, store-internal, non-UTF-8, repeated-separator, `.` or `..`, broken, directory, oversized, and multi-hop inner targets remain invalid. A recognized outer Home Manager link retains Home Manager ownership when its leaf is broken or unsafe, so Doctor continues to route remediation to the declarative owner.

The runtime trust boundary is the immutable Nix store. Inspection captures and validates each raw target, then opens the captured inner target instead of reopening the mutable home link. A user capable of replacing an already-validated parent or leaf inside the production Nix store could also replace the installed binary; writable injected stores exist only in tests. Do not add recursive resolution, retry loops, Nix-daemon access, or platform-specific `O_NOFOLLOW` code for that out-of-scope trust failure.

### Project-scope diagnostics

Project-scope symlinks remain unsupported without inspecting their targets. A project candidate whose path lookup fails because an ancestor is not a directory is an unsupported topology, not an unreadable file. Map `NotADirectory` to `unsupported_topology`; preserve `unreadable` for permission and I/O failures.

The zero-byte `nix-configs/.codex` regular file therefore produces project-scoped unsupported-topology evidence for `.codex/hooks.json`. A regular project `.claude/settings.json` remains imperative. Once the global Home Manager leaf is valid, global Claude plus project Claude remains `Duplicate` / `Mixed` and uses the existing duplicate-scope remediation.

### Mutation boundary

Do not change `read_managed_file`, staging, application, removal, compare-and-swap checks, rollback, recovery, or journal handling. All imperative paths continue to reject every symlink, including the newly accepted read-only Home Manager shape.

## Doctor behavior

- Valid global Home Manager definitions pass semantic comparison for Codex, Claude, and Antigravity.
- Unsafe inner targets fail with Home Manager ownership and declarative remediation.
- A non-directory project ancestor fails with the exact project path, project scope, unsupported ownership, and `unsupported_topology`.
- A valid global definition never masks invalid, stale, or duplicate project evidence.
- Codex runtime trust remains a separate advisory.

No new public state, configuration, CLI option, or provider schema is introduced.

## Test strategy

Follow test-driven development:

1. Add a separate production-shaped helper that creates a regular immutable source plus an absolute store leaf symlink for each provider. Add one all-provider test and verify it fails under current inspection before implementation.
2. Preserve the existing direct regular-leaf helper and coverage. Split the old generic nested-symlink rejection into accepted store-leaf and focused rejection cases for relative, non-store, store-internal, malformed, broken, directory, multi-hop, and symlinked-parent inner targets.
3. Add a project fixture whose `.codex` ancestor is a regular file and assert `unsupported_topology`, plus a valid global Home Manager Claude definition with a regular project Claude duplicate.
4. Make `home-manager-doctor-fixtures.nix` produce store symlink leaves instead of copied regular files. The VM must verify all three provider rows and the project-conflict diagnostics using the packaged binary.
5. Retain mutation tests proving init and removal reject recognized Home Manager and arbitrary links.

## Verification

Run focused provider-hook and Doctor tests after each red-green cycle, then run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo build
nix fmt -- --check .
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).home-manager-module --no-link
```

Run the required packaged storage-security VM check because the regression depends on real Nix-store ownership and link topology. These commands establish a verified repository candidate, not production acceptance.

Live acceptance is a separate, explicitly authorized gate: rebuild and activate the user's Home Manager generation, then run installed `cbrain doctor --json` from `nix-configs` and verify all three global provider rows plus the Codex and Claude project diagnostics. Do not claim live acceptance from repository CI or the VM.

## Non-goals

- Accepting arbitrary or recursive symlink chains.
- Accepting project-scope declarative links.
- Changing generated provider definitions or Home Manager options.
- Repairing or deleting conflicting project files automatically.
- Weakening imperative link protections.

## Acceptance criteria

- Production-shaped global Home Manager Codex, Claude, and Antigravity definitions pass read-only inspection.
- Unsafe, stale, malformed, provider-mismatched, or unreadable targets remain fail-closed with accurate ownership.
- The Codex non-directory project ancestor reports `unsupported_topology`; the Claude regular duplicate remains visible as mixed ownership.
- Imperative init, removal, rollback, and recovery retain strict no-follow behavior.
- Focused Rust tests, the full Rust gates, Nix checks, and the packaged VM pass before completion is claimed.

## Stress Test Results: Home Manager provider leaf indirection

### Resolved Decisions

- Home Manager ownership comes from the exact outer global link into `*-home-manager-files/<provider suffix>`, matching Home Manager's installed collision checker. The inner target affects safe readability, not ownership.
- Read-only inspection supports the existing direct regular leaf and exactly one additional absolute store-to-store leaf hop. Recursive and arbitrary chains remain invalid.
- An inner target must remain under the same store, begin beneath a top-level store object rather than an internal namespace, traverse only real directories, and end at a bounded regular non-symlink file.
- Doctor stays offline and dependency-free: it does not query Home Manager profiles, the Nix database, xattrs, or the Nix daemon.
- `NotADirectory` on a project candidate is `unsupported_topology`; real permission and I/O failures remain `unreadable`.
- The immutable production Nix store is the TOCTOU trust boundary. The reader opens the captured inner target and does not reopen the mutable home link.
- Candidate verification and live Home Manager acceptance are distinct gates; activation requires explicit authorization.

### Changes Made

- Separated ownership recognition from inner content-read validation.
- Excluded Nix store-internal namespaces from accepted inner targets while retaining nested paths within a normal store object.
- Preserved direct-leaf coverage and added a separate two-hop red test before implementation.
- Made the packaged two-hop VM and live post-activation Doctor checks explicit.

### Deferred / Parking Lot

- No generalized Home Manager ownership API is added. If Home Manager changes its outer generation-link convention, Doctor will continue to fail closed until a new shape is deliberately supported.
- Live activation and installed Doctor acceptance wait for explicit deployment authorization after repository verification.

### Confidence Assessment

- Overall: High.
- Remaining concern: ownership intentionally follows Home Manager's current `*-home-manager-files/*` convention; the packaged VM must prevent future fixture drift from production topology.
