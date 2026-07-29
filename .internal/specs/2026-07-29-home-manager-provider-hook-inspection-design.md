# Home Manager provider hook inspection

## Goal

`coding-brain doctor` must validate provider hooks installed by the exported Home Manager module without weakening the symlink protections used by `coding-brain init` and `coding-brain init --remove`.

The supported declarative shape is a global provider file whose home-directory path is a symbolic link to the matching path inside a `/nix/store/*-home-manager-files` generation:

| Provider | Link path | Required target suffix |
| --- | --- | --- |
| Codex | `~/.codex/hooks.json` | `.codex/hooks.json` |
| Claude | `~/.claude/settings.json` | `.claude/settings.json` |
| Antigravity | `~/.gemini/config/hooks.json` | `.gemini/config/hooks.json` |

Project-local symlinks and symlinks outside this exact Home Manager shape remain invalid.

## Root cause

Read-only provider inspection currently calls the same staging path used by imperative installation. That path reads files through `read_managed_file`, which deliberately accepts only regular non-symlink files. `inspect_provider_hook_at` converts the resulting error to `ProviderHookInspection::Invalid`, so Doctor reports every Home Manager-managed provider file as unsafe and recommends an imperative repair that cannot own the path.

The mutation rule is correct. Inspection needs a separate read policy and an ownership result.

## Design

### Separate reading from semantic comparison

Extract the provider-specific JSON merge and comparison work from `stage_provider_hooks_with` into a helper that receives already-read bytes. Both mutation and inspection reuse this semantic helper, so current, stale, missing-definition, malformed, and provider-mismatched classification continues to use explicit provider contracts.

The contract is ownership-aware only after the read policy establishes ownership. Imperative mutation and regular-file inspection use the existing imperative contract. A structurally recognized Home Manager file uses the existing declarative contract: Claude's generated permission handler omits the optional status message, and Antigravity's generated commands use the immutable package executable. This does not make those forms interchangeable for regular files and does not normalize arbitrary commands; it accepts only the exact shipped definition for the already-recognized declarative owner.

Mutation staging keeps `read_managed_file` unchanged. Installation, removal, transaction preconditions, rollback, and recovery therefore continue to reject all symlinks.

Inspection uses a new read-only helper:

1. A missing path returns no bytes and no owner.
2. A regular non-symlink file is read with the existing 1 MiB limit and is marked imperative.
3. A symlink is accepted only for a global provider path when its absolute target contains only root and normal components, is under `/nix/store`, has exactly one generation directory ending in `-home-manager-files`, and the remaining target path exactly matches the provider's expected home-relative path. Inspection rejects `.` and `..` components before any target is followed. The store root, generation directory, and every suffix directory through the target parent must each be a real non-symlink directory; an ancestor symlink does not establish Home Manager ownership.
4. The exact target captured from `read_link` is opened directly. The target path must not itself be a symlink, metadata from the opened file handle must describe a regular file, and its contents must remain within 1 MiB.
5. Any other file type, target shape, missing target, or read failure is invalid. A recognized Home Manager link retains declarative ownership even when its target is malformed or stale, allowing Doctor to give the correct repair boundary.

Opening the captured target rather than reopening the home-directory link prevents a concurrent link change from redirecting the inspection after validation.

### Carry ownership through aggregation

Provider inspection returns both the existing state and an ownership classification:

- absent, when no managed file exists;
- imperative, for regular files;
- Home Manager, for one or more recognized declarative files;
- mixed, when declarative and imperative definitions coexist across global and project scopes;
- unsupported, for an arbitrary symlink or another file type that neither supported owner may mutate.

State precedence remains unchanged: invalid precedes stale, then the number of current definitions determines missing, current, or duplicate. Ownership is aggregated across every present candidate, so a current global Home Manager definition plus a current project definition remains duplicate and is reported as mixed ownership. An unsupported project-local symlink beside a valid global Home Manager definition remains invalid and is not misattributed to Home Manager.

No paths or file contents enter Doctor's human or JSON output.

### Doctor remediation

Current definitions retain the existing pass or provider-unavailable result. Non-current imperative installations retain the existing `coding-brain init <provider>` repair hint.

When only Home Manager ownership is present, Doctor does not recommend imperative mutation. Its hint tells the operator to repair the Home Manager-owned provider definitions in the Nix configuration, rebuild the Home Manager generation, and rerun Doctor. Mixed supported ownership tells the operator to remove the duplicate scope from either Home Manager or the regular provider configuration. Unsupported ownership tells the operator to replace the unsafe link or file type before rerunning setup.

The separate `Codex hook trust` check remains advisory with `review /hooks` guidance. Its wording continues to describe runtime trust only and does not call valid declarative definitions unsafe.

## Failure and security behavior

- Arbitrary, relative, project-local, cross-provider, and non-store symlinks remain invalid.
- A recognized Home Manager link to a directory, nested symlink, missing target, oversized file, invalid JSON value, or wrong provider definition does not become current.
- Broken Home Manager targets, malformed JSON, oversized files, directory targets, and nested symlinks are invalid. Valid JSON without a managed definition is missing; managed commands with the wrong executable, arguments, provider, event, or shape are stale.
- A valid global definition never masks a higher-precedence invalid or stale project candidate.
- Read-only acceptance never reaches transaction preparation or replacement.
- Imperative compare-and-swap hashes and non-symlink checks remain unchanged.
- Recognition is structural and bounded; Doctor does not execute Nix or invoke Home Manager to inspect ownership.

## Tests

Provider-hook unit tests will cover all three providers with a temporary injected Nix-store root:

- matching Home Manager links classify as current and declarative;
- stale command, malformed JSON, missing managed definition, provider mismatch, and oversized target preserve accurate state and declarative ownership;
- global declarative plus project imperative definitions classify as duplicate with mixed ownership;
- a valid global declarative definition plus an unsupported or stale project candidate retains the higher-precedence failure and correct source classification;
- wrong target suffix, wrong provider suffix, relative target, nested symlink, directory target, missing target, and non-store target classify as invalid;
- existing regular-file missing/current/stale/invalid/duplicate tests continue to pass;
- `stage_provider_hooks_at` and removal staging still reject both recognized Home Manager links and arbitrary symlinks.

Doctor tests will verify:

- current declarative definitions pass without an imperative repair hint;
- stale or invalid declarative definitions fail with Home Manager remediation;
- duplicate mixed ownership is advisory with ownership-aware remediation;
- regular-file states keep their current messages and `coding-brain init <provider>` hints;
- Codex trust remains a separate advisory.

The Nix Home Manager check will build one `home-manager-files` fixture containing the evaluated Codex hooks, Claude settings, and generated Antigravity hooks at their real home-relative paths. It will link a temporary home directory to those three store files and run the packaged `coding-brain doctor --json` with fake provider executables. The test captures JSON even if an unrelated Doctor check makes the process exit nonzero, then asserts that all three provider setup rows are current, none has an imperative repair hint, and Codex trust remains a separate advisory. No module option or generated hook definition changes.

The real packaged fixture exposed that the shipped Claude and Antigravity declarative definitions are not byte-for-byte the imperative definitions. The ownership-aware contract above is therefore required by the acceptance fixture; changing those generated definitions would move the ownership boundary and is explicitly out of scope.

## Verification

Run focused provider-hook and Doctor tests first, then:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).home-manager-module --no-link
```

Use the repository's Nix development shell if the bare Rust toolchain cannot resolve the locked workspace environment.

## Non-goals

- Allowing `init` or `uninit` to replace a Home Manager symlink.
- Managing Home Manager configuration from Doctor.
- Proving that Codex has trusted a rendered hook.
- Supporting arbitrary symlinked provider configuration.
- Changing provider hook schemas or Home Manager module options.

## Acceptance criteria

- Matching Home Manager-managed Codex, Claude, and Antigravity files are reported as current.
- Declarative and imperative reads have separate symlink policies, while all mutation paths retain strict non-symlink enforcement.
- Declarative stale, malformed, missing-definition, duplicate, and provider-mismatched cases remain distinguishable.
- Doctor routes declarative remediation to Home Manager and never to `coding-brain init <provider>`.
- Codex runtime trust remains a distinct advisory.
- Existing regular-file behavior and coverage remain intact.

## Stress Test Results: Home Manager provider hook inspection

### Resolved Decisions

- Declarative ownership requires an exact global provider target under `/nix/store/*-home-manager-files`; custom store roots fail closed until they have trustworthy provenance.
- Target recognition rejects non-normal path components before prefix and suffix matching; it does not canonicalize an untrusted target first.
- Inspection opens the captured store target instead of reopening the home link, validates the opened file handle, and retains the 1 MiB bound.
- Unsupported symlinks and file types are distinct from imperative and Home Manager ownership so a valid global definition cannot misattribute a project failure.
- Mutation and inspection have separate readers. They share only pure provider-definition comparison, and inspection cannot produce an applicable edit.
- Doctor remediation follows ownership, while Codex runtime trust remains a separate `/hooks` advisory.
- Failure precedence and classification remain explicit: invalid, then stale, then current-count aggregation.
- Inspection stays bounded and dependency-free; a changed Home Manager topology fails closed.
- Verification combines exhaustive Rust tests with a real Nix-store Home Manager fixture and packaged Doctor JSON.

### Changes Made

- Added unsupported ownership and ownership-aware aggregation.
- Required metadata validation on the opened target file.
- Required lexical normalization before Home Manager ownership recognition.
- Made failure classifications and mixed-scope precedence explicit.
- Specified ownership-specific Doctor wording.
- Made the Nix integration fixture resilient to unrelated Doctor exit status.

### Deferred / Parking Lot

- Custom Nix store roots remain unsupported.
- Future Home Manager target topologies require deliberate recognition and tests.

### Confidence Assessment

- Overall: High.
- Areas of concern: ownership recognition intentionally depends on the current Home Manager `home-manager-files` target shape.
