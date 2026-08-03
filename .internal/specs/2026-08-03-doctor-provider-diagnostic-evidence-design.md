# Doctor provider diagnostic evidence

## Goal

`cbrain doctor` must explain every non-current provider setup result with the provider files that produced it. Human output must remain concise, while `doctor --json` exposes stable structured evidence. The change must not alter provider-hook classification, Home Manager ownership recognition, or imperative mutation safety.

## Root cause

Read-only provider inspection currently classifies each global or project candidate, but immediately reduces those classifications to aggregate `state` and `ownership` fields. Candidate paths and scopes are discarded, and read, topology, parse, and contract failures all collapse to `ProviderHookState::Invalid`. Doctor therefore cannot identify the inspected files, the two definitions behind a duplicate, or the reason for an invalid result.

The existing aggregate classification and repair routing are correct. The missing behavior is preservation and presentation of bounded per-file evidence.

## Design

### Typed inspection evidence

Extend `ProviderHookInspection` with a finite deterministic list of records, one for each deduplicated candidate path inspected for that provider. Each record contains:

- `path`: the inspected provider configuration path, represented as a JSON-safe string;
- `path_lossy`: true when non-UTF-8 path bytes required replacement in `path`;
- `scope`: `global` or `project`;
- `ownership`: `absent`, `imperative`, `home_manager`, or `unsupported` for that file;
- `state`: `missing`, `current`, `stale`, or `invalid` for that file;
- `reason`: an optional closed diagnostic reason.

The aggregate `state` and `ownership` fields remain and continue to drive Doctor severity and remediation. They are derived exclusively from the per-file records rather than calculated along a parallel path, preventing evidence from disagreeing with the final classification. Aggregation retains the existing precedence: any invalid candidate wins, otherwise any stale candidate wins, otherwise the number of current candidates determines missing, current, or duplicate. Ownership aggregation remains unchanged, including mixed ownership when declarative and regular definitions coexist.

Candidate discovery also remains unchanged. Codex and Claude inspect the deduplicated global path plus applicable project roots; Antigravity inspects only its global path. Missing candidates remain evidence so a non-current result identifies every relevant location Doctor considered. A home/project alias remains one record. Record order follows inspection order: global first, then project directories from the detected root toward the current directory. No arbitrary cap, re-sort, or omission may hide a relevant candidate.

### Stable Doctor output

Add optional structured evidence to `Check` with Serde defaulting and omission when absent. Provider setup checks populate it whenever the final `ProviderSetupState` is not `Current`, including current hook definitions whose provider executable is unavailable. Only final-current/pass provider rows and unrelated Doctor checks retain their existing JSON shape, and previously serialized checks without `evidence` continue to deserialize.

The JSON shape is:

```json
{
  "name": "Claude setup",
  "status": "advisory",
  "message": "degraded: managed definitions are duplicated across scopes",
  "fix_hint": "...",
  "evidence": {
    "provider_files": [
      {
        "path": "/home/example/.claude/settings.json",
        "path_lossy": false,
        "scope": "global",
        "ownership": "home_manager",
        "state": "current"
      },
      {
        "path": "/work/project/.claude/settings.json",
        "path_lossy": false,
        "scope": "project",
        "ownership": "imperative",
        "state": "current"
      }
    ]
  }
}
```

Optional `reason` is omitted when it does not apply. `evidence` is omitted from passing/current provider rows and checks without provider-file evidence.

Human rendering adds one indented line per provider file after a non-current provider row and before its repair hint. Each line includes scope, escaped path, ownership, state, and reason when present. Control and bidirectional-format characters in paths are escaped before terminal output while ordinary Unicode remains readable. JSON uses the full lossy UTF-8 path and explicitly marks replacement with `path_lossy`; raw path bytes are not exposed. File contents, commands, symlink targets, and raw errors are never rendered.

### Bounded diagnostic reasons

Reasons use a closed snake-case enum:

- `unsupported_topology`: the path is an unsupported file type or symlink topology;
- `unreadable`: metadata or bounded content could not be read, including an oversized file;
- `malformed_content`: content is invalid JSON, has the wrong root type, or fails provider-specific structural processing;
- `contract_mismatch`: readable structured content does not match the managed provider contract.

Missing and current records have no reason. A stale record uses `contract_mismatch`. All comparison errors map to `malformed_content`; `contract_mismatch` is reserved for content that parses and processes successfully but contains a managed definition that differs from the expected contract. An invalid filesystem record preserves `unsupported_topology` or `unreadable`.

The reason never contains operating-system error text, file content, parsed values, commands, or other attacker-controlled detail. It is therefore bounded and non-sensitive by construction.

## Security and failure behavior

- Read-only inspection remains the only producer of evidence; Doctor does not re-inspect or reinterpret paths.
- Existing Home Manager recognition continues to require the exact supported global `/nix/store/*-home-manager-files` topology.
- Arbitrary, relative, project-local, cross-provider, nested, or non-store symlinks remain invalid.
- Imperative install, removal, transaction, rollback, and recovery paths continue to use strict non-symlink reads and do not consume diagnostic evidence.
- Evidence cannot promote or downgrade a state. Existing aggregate precedence and ownership-specific remediation remain authoritative.
- Full inspected paths are present because identifying the offending file is an acceptance requirement. Lossy conversion is disclosed instead of pretending byte-exactness. JSON escaping and terminal control- and bidi-character escaping prevent paths from injecting or visually rewriting output structure.

## Tests

Provider inspection tests will cover:

- a Home Manager global Codex definition plus a regular project Codex definition, including both paths, scopes, owners, and current states;
- the equivalent mixed Claude case;
- an invalid Home Manager Antigravity definition with declarative ownership and the correct bounded reason;
- unsupported topology, unreadable input, malformed content, and contract mismatch reason mapping;
- deduplicated home/project aliases and unchanged aggregate failure precedence;
- the existing arbitrary/project symlink rejection and imperative mutation protections.

Doctor tests will cover:

- evidence appears only for non-current provider setup results;
- duplicate results serialize both conflicting definitions in deterministic order;
- JSON uses stable typed fields and does not expose file contents, commands, symlink targets, or raw errors;
- human output contains concise escaped evidence lines;
- current provider rows remain unchanged and omit evidence;
- legacy serialized checks without `evidence` still deserialize.

CLI integration tests will exercise actual `doctor --json` output for mixed Home Manager/global and regular project definitions plus invalid Home Manager Antigravity content. The structured evidence must match the classifier tests rather than a separate CLI interpretation.

Focused tests run through `nix develop path:.` because the bare shell does not have a configured Rust toolchain. Final verification runs formatting, workspace tests, Clippy with warnings denied, and a workspace build. Existing Nix Home Manager checks remain part of regression verification when practical.

## Non-goals

- Changing provider hook schemas or accepted definitions.
- Accepting any new symlink or Home Manager topology.
- Changing setup severity, failure precedence, or remediation ownership.
- Exposing raw filesystem or parser errors.
- Adding evidence to passing/current provider rows.
- Changing the unrelated Brain session-management warning.

## Acceptance criteria

- Every non-current provider setup result identifies each relevant provider file path, scope, ownership, and per-file state.
- Invalid and stale records include a bounded reason distinguishing unsupported topology, unreadable input, malformed content, and contract mismatch.
- Duplicate results identify every conflicting definition and scope.
- Human and JSON output carry equivalent evidence, with stable structured JSON fields and concise safe terminal rendering.
- Mixed Home Manager/global and regular project Codex and Claude definitions, plus invalid Home Manager Antigravity definitions, have regression coverage.
- Existing fail-closed symlink handling and imperative mutation protections remain unchanged.

## Stress Test Results: Doctor provider diagnostic evidence

### Resolved Decisions

- Per-file records are canonical; aggregate state and ownership are derived exclusively from them.
- Optional evidence is Serde-defaulted and omitted when absent, preserving existing JSON for passing and unrelated checks and legacy deserialization.
- Paths use full lossy UTF-8 plus an explicit `path_lossy` marker; human output escapes control and bidirectional-format characters without exposing raw bytes or symlink targets.
- Comparison errors are malformed content; contract mismatch requires successful structural processing followed by a managed-definition mismatch.
- Inspection predicates and branch order remain unchanged, while mutation staging and strict non-symlink readers remain untouched.
- Evidence preserves global-to-project inspection order and every deduplicated candidate without an arbitrary cap.
- Verification spans classifier, Doctor, and CLI layers, including legacy JSON compatibility and existing mutation-safety regressions.

### Changes Made

- Replaced the inaccurate bounded-list claim with a finite deterministic ordering contract.
- Made aggregate derivation, backward-compatible serialization, lossy-path disclosure, exact reason mapping, and CLI coverage explicit.

### Deferred / Parking Lot

- No additional topology, provider schema, or raw-path encoding support is introduced.
- The unrelated Brain session-management warning remains out of scope.

### Confidence Assessment

- Overall: High.
- Areas of concern: deeply nested projects can produce many evidence lines, but dropping candidates would violate the diagnostic completeness requirement and the existing scan is already finite by path depth.
