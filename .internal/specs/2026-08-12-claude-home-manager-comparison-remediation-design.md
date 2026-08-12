# Claude Home Manager comparison and remediation

## Goal

`cbrain doctor` must accept the installed Home Manager Claude definition when all eight cbrain hooks match the running package, even when the settings file also contains an unrelated empty hook matcher. A project settings file with no cbrain definition must remain visible in diagnostic evidence without turning the aggregate ownership into `Mixed` or producing duplicate-scope remediation. Current provider rows must retain that non-current per-file evidence, and Codex hook trust must reflect Codex's authoritative trust status instead of warning for every enabled definition.

The change must preserve exact managed-hook validation, failure precedence, and the strict no-follow policy used by imperative init and removal.

## Installed reproduction

The installed `cbrain 0.59.1` reads the global Claude settings through the supported two-hop Home Manager store topology. The file contains all eight expected cbrain commands with the correct executable, provider, matcher, and timeout. It also contains an unrelated empty `Stop` matcher before the cbrain `Stop` matcher; the coding-brain Home Manager module contributes its matcher with `lib.mkAfter`:

```json
{
  "hooks": [],
  "matcher": ""
}
```

The project `.claude/settings.json` is a regular file containing only an unrelated `bd prime --hook-json` `SessionStart` hook.

Doctor currently reports the global file as `home_manager/stale/contract_mismatch` and the project file as `imperative/missing`, then reduces those owners to `Mixed` and recommends removing a duplicate scope. There is only one cbrain definition, so both the global classification and the remediation are wrong.

## Root cause

Home Manager Claude comparison augments the declarative permission handler with the imperative-only status message, runs the normal merge routine, and requires the resulting JSON to equal the augmented input. `merge_nested_hooks` removes every matcher whose `hooks` array is empty, including entries that were empty before cbrain inspected the file. The comparison therefore changes unrelated configuration and concludes that the otherwise exact Home Manager definition is stale.

Ownership aggregation independently folds the filesystem owner of every inspected file. A regular project file is therefore treated as imperative ownership even when its file state is `Missing`, which means it contains no cbrain definition. Combining that record with a Home Manager global file yields `Mixed` and selects duplicate-scope remediation for failures that belong only to the global definition.

Two review findings expose narrower follow-on defects. First, `check_provider_setup` suppresses all file evidence when the aggregate setup is current, hiding the definition-free project record that the design requires Doctor to retain. Second, ignoring every `Missing` record would also discard Home Manager ownership for a declarative file that exists but lacks cbrain definitions; Doctor would then recommend imperative `cbrain init` even though the mutation path correctly refuses to replace the Home Manager symlink.

Codex trust discovery has an independent false-positive implementation: `trust_unverified` is set whenever any enabled managed command is discovered. It never reads trust state. The installed Codex 0.147.0 `hooks/list` response reports all eight global cbrain hooks as `trusted`, so the unconditional `trust unverified; review /hooks` advisory is factually wrong.

## Design

### Preserve unrelated empty matchers

Change `merge_nested_hooks` so it distinguishes a matcher that was already empty from one made empty by removal of an exact cbrain handler.

- Preserve a matcher whose handler list was empty before the merge.
- Remove a matcher when it had handlers and removing exact managed handlers leaves it empty.
- Continue preserving non-cbrain handlers and modified cbrain candidates.
- Continue inserting a missing exact managed definition when no managed collision exists.

This corrects the comparison at the shared source: the merge routine no longer rewrites unrelated empty provider configuration. Missing, stale, modified, or extra managed definitions still change the comparison or populate `preserved_modified_entries`, so they do not become current.

### Aggregate ownership from definitions and failures

Keep each `ProviderHookFileInspection` record unchanged, including the filesystem ownership of a regular file whose managed state is `Missing`. Change only the aggregate ownership fold:

- Ignore ownership only for records that are both `ProviderHookFileState::Missing` and `ProviderHookOwnership::Imperative`.
- Include ownership for `Current`, `Stale`, and `Invalid` records.
- Retain `HomeManager` ownership for a declarative file whose semantic state is `Missing`.

A definition-free regular file therefore remains visible as `imperative/missing` evidence but does not claim ownership of the provider setup. If it is the only candidate, aggregate ownership is `Absent`, and the existing imperative setup hint remains appropriate because init can safely merge into the regular file. A definition-free Home Manager symlink remains `HomeManager/Missing`, so remediation stays declarative. Malformed, unreadable, or unsupported files remain ownership-bearing failures because Doctor cannot safely establish that they contain no managed definition.

State aggregation does not change. Invalid still wins over stale; otherwise the number of current definitions determines missing, current, or duplicate.

### Doctor provider behavior

The existing state and ownership mapping remains the single remediation source after the aggregate ownership correction:

- exact Home Manager global plus unrelated project settings: `Current / HomeManager`;
- stale Home Manager global plus unrelated project settings: `Stale / HomeManager`, with declarative repair;
- exact Home Manager global plus an actual current project cbrain definition: `Duplicate / Mixed`, with duplicate-scope repair;
- unsupported or invalid project candidate beside a valid global definition: the existing fail-closed failure and owner-specific repair.

Provider setup evidence is present whenever any inspected file is non-current, even if the aggregate provider state is current. An exact Home Manager global definition plus an unrelated imperative project file therefore remains a passing provider row while exposing the project file as `imperative/missing` evidence. A fully current set retains the existing compact row with no evidence.

### Codex trust behavior

Use Codex's own bounded app-server `hooks/list` query as the trust authority. The response already applies Codex's effective configuration layering, linked-worktree source selection, positional hook keys, normalized hashes, and trust policy.

- Start `codex app-server --stdio`, complete the required `initialize` and `initialized` handshake, and request `hooks/list` for the active working directory.
- Bound startup, handshake, response parsing, and child cleanup with a short deadline so Doctor cannot hang on a broken Codex installation.
- Filter enabled command hooks whose commands are discoverable cbrain lifecycle, permission, or recovery definitions.
- Return Pass when every enabled cbrain hook is `trusted` or `managed`.
- Return Advisory and name affected events when any enabled cbrain hook is `untrusted` or `modified`.
- Return Advisory with an unavailable reason when Codex is absent, the app-server method is unsupported, the process times out, or the response is malformed.
- Return Skipped only when the authoritative response contains no enabled cbrain definitions.

Do not reproduce Codex's trust hash locally. That would copy a version-sensitive normalization contract and miss effective-layer and linked-worktree behavior that `hooks/list` already owns.

No new diagnostic state, JSON field, CLI option, or configuration field is required.

## Security and failure behavior

- Home Manager ownership recognition and its bounded one-hop store-leaf reader do not change.
- Exact executable, provider, flag, matcher, timeout, and handler-shape checks remain required.
- Modified managed handlers remain stale and are never normalized into acceptance.
- Invalid JSON, malformed nested-hook structure, unreadable files, unsupported topology, and unsafe symlinks remain failures.
- Project-scope symlinks remain unsupported.
- Imperative staging, removal, compare-and-swap, rollback, recovery, and journal handling do not consume aggregate ownership and remain unchanged.
- Ignoring ownership is limited to a successfully parsed regular file for which semantic comparison found no managed cbrain definition. It does not apply to invalid or uncertain input.
- Codex app-server failures are fail-closed as advisory trust-unavailable results, never as trusted.
- The trust probe performs no config write or trust mutation and never auto-approves hooks.

## Test strategy

Follow test-driven development.

1. Add a merge regression with a pre-existing empty unrelated matcher and assert that the matcher survives while exact cbrain definitions remain unchanged.
2. Add a production-shaped Home Manager Claude fixture containing all eight exact definitions plus the empty `Stop` matcher. Assert `Current / HomeManager`.
3. Add aggregation coverage for a Home Manager global definition plus a regular project file containing only an unrelated hook:
   - exact global produces `Current / HomeManager`;
   - stale global produces `Stale / HomeManager`;
   - the project record remains `imperative/missing` in file evidence.
4. Retain and explicitly verify the true duplicate case in which both global and project files contain current cbrain definitions, producing `Duplicate / Mixed`.
5. Add Doctor assertions that the stale-global case recommends Home Manager repair and never duplicate-scope removal.
6. Add a current-provider-row assertion that the unrelated project file remains in evidence.
7. Add a Home Manager `Missing` ownership assertion that preserves declarative remediation.
8. Add Codex trust result tests for all-trusted, managed, untrusted, modified, absent, malformed, unavailable, and timeout responses using an injected probe boundary; keep one bounded subprocess protocol test where practical.
9. Keep existing unsafe topology, malformed content, provider mismatch, mutation rejection, and transaction tests unchanged and passing.

## Verification

Run the focused red-green tests first, then the repository gates:

```bash
nix develop path:. --command cargo test provider_inspection -- --test-threads=1
nix develop path:. --command cargo test provider_setup -- --test-threads=1
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
nix fmt -- --check .
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).home-manager-module --no-link
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).storage-security-vm --no-link
```

The repository and packaged checks establish a candidate. Installed acceptance remains a separate gate: after explicit authorization, rebuild and activate the Home Manager generation, run installed `cbrain doctor --json` from `nix-configs`, and confirm the global Claude row is current while the unrelated project file causes no duplicate remediation.

## Non-goals

- Changing Home Manager-generated Claude settings.
- Removing or rewriting the user's unrelated empty matcher or `bd prime` hook.
- Hiding per-file evidence for definition-free project settings.
- Accepting modified, partial, stale, or provider-mismatched managed hooks.
- Weakening symlink or imperative mutation protections.
- Repairing the separate `nix-configs/.codex` non-directory ancestor.
- General order-insensitive normalization of hand-reordered provider matcher arrays outside the production `lib.mkAfter` shape.
- Writing Codex `trusted_hash` values, approving hooks, or duplicating Codex's hash algorithm.

## Acceptance criteria

- The production-shaped global Claude definition with an unrelated empty matcher is current.
- A regular project file without a cbrain definition remains `imperative/missing` evidence but does not create mixed ownership.
- That per-file evidence remains visible even when the aggregate provider row is current.
- A Home Manager file with no cbrain definition retains Home Manager ownership and declarative remediation.
- A stale global Home Manager definition routes remediation to Home Manager when the project file has no cbrain definition.
- A genuine global/project duplicate remains `Duplicate / Mixed` with duplicate-scope remediation.
- Codex hook trust passes for authoritative `trusted` or `managed` definitions and advises only for untrusted, modified, or unavailable trust state.
- All unsafe and uncertain inputs remain fail-closed, and imperative mutation safety is unchanged.
- Focused, serial workspace, formatting, Clippy, build, Home Manager module, and packaged VM gates pass before installed acceptance is requested.
