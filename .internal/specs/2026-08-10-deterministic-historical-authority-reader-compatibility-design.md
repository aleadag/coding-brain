# Deterministic Historical Authority Reader Compatibility

- Status: Approved and stress-tested
- Date: 2026-08-10
- Bead: `codexctl-dzlb9.16`
- Brainstorm session: `codexctl-dzlb9.16.1`

## Context

The SQLite migration accepts terminally correlated legacy permission proposals
from three source families. It stores their canonical decision sources as
`model`, `deterministic_safety`, or `native_provider` and publishes each row as
historical authority with `response_eligible = false` and
`delivery_state = 'unknown'`.

The historical reader does not apply the same source policy. It requires every
historical decision identity to have source `model`, then reconstructs a model
identity for activity validation. A valid migrated deterministic deny therefore
makes the completed database unreadable through `learning_read_session`, the
non-interactive Brain review queue, and the TUI SQLite refresh path.

The production row that exposed the mismatch is
`dec_1786178933696120619_4155783_0`. It is a Codex deny with canonical source
`deterministic_safety`, proposal-terminal provenance, terminal source cursor
1189, no live permission attempt, response eligibility disabled, and delivery
unknown. Its terminal activity agrees on provider, session, turn, action, and
cursor. The reader rejects the source before that valid relationship can be
used.

A separate operator experiment moved the complete `db/` directory. Because
that directory also held the migration state and frozen manifest, startup
attempted a fresh migration against already-frozen legacy sources and failed
the legacy guard mode check. That recovery case is not the same defect and is
outside this change.

## Chosen Approach

Introduce one `pub(super)` canonical historical-source type in
`storage/decisions.rs`, beside `DecisionIdentity`, with exactly these values:

- `model`
- `deterministic_safety`
- `native_provider`

The type owns exact parsing and canonical serialization. Unknown, absent, or
otherwise malformed values remain invalid storage.

Keep legacy spellings local to migration through one conversion function used
by both initial import and replay/accounting validation. The function returns
the canonical type rather than a string:

| Legacy proposal source | Canonical historical source |
| --- | --- |
| `model`, `brain` | `model` |
| `deterministic` | `deterministic_safety` |
| `provider_policy` | `native_provider` |

Every other legacy proposal source remains an error. Both migration paths use
the same conversion so a successfully imported source cannot later fail exact
accounting because the mappings drifted.

Historical-authority validation parses the source stored in
`decision_identities` and reconstructs the validation identity with that exact
canonical source. It must not substitute `model`. The existing activity and
authority relationship checks remain unchanged.

Do not replace the string field in the public `DecisionIdentity` type. A global
source refactor would touch live permission behavior without improving this
compatibility boundary.

## Authority and Validation Invariants

Migrated historical rows remain closed evidence. The migration must not create
`permission_attempts` or `permission_commits`; historical rows remain response
ineligible with delivery unknown and cannot satisfy response or delivery APIs.
This applies to historical deterministic allow and deny evidence. It does not
relax the separate live deterministic-safety rule, which permits only a deny
without response delivery.

Historical source validation does not impose an action restriction beyond exact
agreement with the terminal event. Model, deterministic-safety, and native-
provider history may contain either action; their closed historical status, not
their source label, prevents live authority.

Historical validation continues to verify:

- decision kind, provider, session, turn, and tool-use identity;
- authority action and the terminal event kind, state, and action;
- terminal cursor range and the activity high-water mark;
- decision ID and source activity correlation;
- proposal-terminal versus correlated provenance identifiers;
- transaction and request-key bounds when correlation requires them;
- response eligibility and delivery state.

The new source policy adds exact source parsing to this list. Schema constraints
remain defense in depth, but application reads must also reject an unknown
source if a database was created or modified with constraints disabled.

## Runtime Fault Contract

`StorageError::fault_category` already classifies `InvalidStorage` as
`corrupt`. The binary currently flattens non-busy SQLite refresh failures into
`BrainSourceError::Other`, even when its rendered string includes a category.
This loses the category at the runtime/TUI boundary.

Keep `BrainSourceError::Busy` as the only busy representation. Add a typed
storage-unavailable variant and a small core-owned category enum for `full`,
`io`, `corrupt`, and `other`. The binary maps its non-busy storage category into
the core type. A storage failure carries that category to the TUI, which
renders the stable diagnostic and keeps the last coherent view. Generic source
errors remain `Other(String)` and continue through the existing bounded,
redacted status path.

The non-interactive review command continues to map `StorageError` directly.
An invalid historical invariant must render `SQLite decision storage
unavailable (corrupt)`, while missing or migration-required storage remains
distinct from corruption as `other`. No raw SQLite error, decision payload,
command, or local path is exposed in the status.

## Verification

### Migration and source policy

Add a legacy migration regression with an exactly terminal-correlated
deterministic allow and deny proposal. After migration:

- the typed identity source is `deterministic_safety`;
- `learning_read_session` and its first page succeed;
- historical authority is response ineligible with delivery unknown;
- no live permission attempt or commit exists;
- admitting a fresh request with matching provider and session context does not
  obtain a historical decision or delivery capability.

The authority-isolation regression combines public behavior with structural
checks: migration creates zero permission attempts and commits, the historical
API returns closed evidence, and a fresh matching admission has neither a
permission state nor decision. The canonical historical-source type is not
exposed through live commit constructors or delivery APIs.

Exercise model, deterministic safety, and native provider through the same
canonical historical validator. Corruption tests bypass schema constraints only
to inject unknown sources or inconsistent authority/activity relationships,
then prove that both audit and learning reads fail closed.

### Production-shaped regression

Create a redacted fixture builder for `dec_1786178933696120619_4155783_0` with
the observed deterministic deny relationship. Set the test database activity
high-water to 1188, representing compacted earlier history, then append the
terminal event through the public API so it receives cursor 1189 with a fully
validated payload. Preserve the identity, null tool-use ID, source, action,
provenance, response eligibility, and delivery state; replace operator-specific
command, project, and path values with neutral bounded values. Add a separate
canonical-source case with a present tool-use ID, and retain exact failure cases
for missing session and provider, session, turn, tool-use, action, or cursor
mismatches.

The production-shaped fixture must reproduce the pre-fix historical read
failure and pass after source-preserving validation. Keep raw legacy migration
coverage at a small cursor rather than adding 1,188 filler JSONL records.

### Review and TUI

Exercise the SQLite-backed non-interactive review queue with deterministic
historical evidence and assert that it loads successfully. Separately inject an
invalid historical invariant and assert the stable `corrupt` category.

Exercise the real SQLite runtime refresh loader, not only a scripted source.
Then cover the TUI error boundary with a prior successful refresh followed by a
typed corrupt storage failure. The complete prior `BrainRefresh` projection,
including its snapshot, review queue, scorecard, review projection, and selected
row, remains intact while the status reports corruption. Completed user actions
and recovery warnings retain their existing higher status priority. Add a cold-
start corruption case, plus a distinct `Other` case for missing or migration-
required storage, so tests cannot treat every storage failure as corruption.

Run focused migration, SQLite decision-read, permission-authority, review,
runtime, and TUI tests, followed by formatting, workspace tests, build, and
Clippy with warnings denied.

## Scope

Expected production changes are limited to the storage source policy, migration
conversion, runtime fault type, and TUI mapping/rendering. Tests may add a
redacted fixture and focused helpers needed to construct canonical historical
rows.

This change does not alter the SQLite schema, export format, live permission
policy, automatic delivery, migration publication protocol, legacy freeze
policy, or whole-directory recovery. It does not repair or rewrite the live
database: the existing canonical data becomes readable because the reader and
migrator agree on supported sources.

The preserved diagnostic directory
`~/.local/state/coding-brain/db.failed-fresh-20260810-2058` is operator state and
will not be modified by implementation or verification.

## Stress Test Results: Deterministic Historical Authority Reader Compatibility

### Resolved Decisions

- Own the canonical historical-source type beside `DecisionIdentity`; migration
  and permission history consume it without changing live identity APIs.
- Use one migration-only legacy conversion that returns the canonical type;
  legacy `brain` never becomes a generally valid SQLite source.
- Accept historical allow and deny actions for every supported source when the
  terminal relationship is exact; closed historical status enforces isolation.
- Preserve the production row's null tool-use ID and cover present and mismatched
  tool-use identities separately.
- Keep Busy as one top-level runtime error path; typed storage-unavailable
  categories are Full, I/O, Corrupt, and Other.
- Verify the complete last coherent projection, cold-start behavior, and status
  precedence instead of checking only the rendered status.
- Prove live-authority isolation through both table cardinality and public
  admission/state APIs.
- Reproduce cursor 1189 through a compacted high-water fixture rather than
  filler activity records.
- Keep whole-directory recovery outside this reader fix because it requires a
  separate authority and rollback design.
- Keep the existing bounded pagination and cursor-index query plan; canonical
  source parsing is constant-time and does not justify a benchmark.

### Changes Made

- Specified source-policy ownership and the typed migration conversion boundary.
- Removed the duplicate Busy representation from the proposed fault taxonomy.
- Strengthened action, tool-use, authority-isolation, fixture, and TUI retention
  coverage.
- Confirmed that the change adds no query, transaction, allocation pass, or
  broader scan; any such implementation change is design drift.

### Deferred / Parking Lot

- Automatic recovery after removal of the canonical database, migration state,
  and frozen manifest remains out of scope and was not classified as a product
  bug by this stress test.

### Confidence Assessment

- Overall: High
- Areas of concern: runtime error-type changes touch core, binary, and TUI
  match sites; the implementation plan must enumerate every compiler-visible
  consumer and preserve generic error redaction and status precedence.
