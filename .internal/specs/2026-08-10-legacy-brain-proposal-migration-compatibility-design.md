# Legacy Brain Proposal Migration Compatibility Design

- Status: Approved
- Date: 2026-08-10
- Bead: `codexctl-dzlb9.13`
- Brainstorm session: `codexctl-dzlb9.13.1`

## Context

The pre-SQLite permission writer stored model-generated hook proposals with
`brain_source: "brain"`. The unified SQLite migration validates those records
as legitimate proposals, but its import and restart-accounting paths accept
only `model`, `deterministic`, and `provider_policy`. A terminally correlated
legacy proposal with source `brain` therefore aborts migration before canonical
database publication.

The historical value describes the same model provenance represented by the
canonical SQLite source `model`. This is a compatibility gap for valid legacy
evidence, not malformed storage.

## Chosen Approach

In both legacy hook-proposal source conversions in
`src/brain/storage/migration.rs`, map only `brain` to canonical `model`. Keep
the existing mappings for `model`, `deterministic`, and `provider_policy`, and
continue rejecting every other source.

Keep the mapping local to migration. Do not normalize the legacy record during
parsing, change export behavior, alter the SQLite schema, or make historical
evidence eligible for live responses.

## Authority and Recovery Invariants

- A proposal is imported only when an exact terminal `Allowed`/`approve` or
  `Denied`/`deny` correlation already makes it authoritative historical
  evidence.
- Incomplete, abstaining, mismatched, and non-terminal proposals continue to be
  counted as incomplete and skipped.
- Imported rows remain `response_eligible = false` with
  `delivery_state = 'unknown'`; they cannot satisfy live permission authority
  APIs.
- Restart accounting applies the same `brain` to `model` conversion as initial
  import so an existing Building generation can be revalidated and resumed.
- Unknown sources remain an `InvalidStorage` error.
- Legacy JSONL and migration state are never rewritten as part of recovery.

## Verification

Add focused regression coverage based on actual pre-cutover hook-proposal
shape:

1. Add a dedicated raw legacy fixture with unique Antigravity allow and deny
   proposals, exact Allowed and Denied terminal events, and
   `brain_source: "brain"`. Do not add journal or lifecycle authority to this
   fixture.
2. Both proposals import with canonical `model` typed decision identities while
   their preserved legacy decision payloads retain the original `brain` source.
   The historical API returns them with delivery unknown, while live
   permission-state and permission-decision lookups remain absent and cannot
   initiate delivery.
3. A focused source-boundary matrix proves that `brain` proposals with no
   terminal, mismatched correlation, or `abstain` remain incomplete and
   non-authoritative. An arbitrary unknown source with an exact terminal
   remains rejected.
4. A Building-state restart with an owned staging database rebuilds and
   completes using the same generation. A Verified-state restart separately
   exercises exact accounting/revalidation and completes.
5. The Building regression compares legacy fixture bytes before and after
   recovery, confirms the canonical database is absent before successful
   resume, and verifies a second resume is idempotently Complete.
6. Exact accounting counts, focused migration tests, and the workspace
   formatting, test, build, and Clippy gates pass. No benchmark is required
   because the mapping adds no I/O, allocation, query, or migration pass.

## Non-goals

- No schema, export, runtime permission, or provider-hook behavior changes.
- No general source aliasing or acceptance of arbitrary historical values.
- No refactoring beyond the two compatibility mappings and their regression
  fixtures/tests.

## Stress Test Results: Legacy Brain Proposal Migration Compatibility

### Resolved Decisions

- Keep the alias explicit in both migration-only matches instead of extracting
  a general source-normalization helper.
- Canonicalize legacy `brain` only as model provenance and only after exact
  terminal correlation.
- Cover both Building rebuild and Verified accounting/revalidation restart
  boundaries without adding in-place partial-import continuation.
- Prove historical readability and live response-authority absence through
  public storage behavior as well as database constraints.
- Retain fail-closed unknown-source handling and show that the new alias cannot
  promote incomplete, abstaining, or mismatched proposals.
- Use a dedicated two-record raw fixture rather than rewriting the unrelated
  permission-journal fixture.
- Keep typed decision identity provenance canonical as `model` while preserving
  the original `brain` value in the immutable legacy decision payload.
- Preserve legacy bytes and the migration generation across automatic Building
  recovery; allow only the owned staging database to be rebuilt.

### Changes Made

- Expanded verification to cover behavioral authority APIs, Building and
  Verified restart boundaries, raw fixture fidelity, negative source cases,
  recovery byte preservation, generation stability, and idempotent resume.
- Explicitly ruled out benchmark work and in-place partial staging continuation
  because neither is required by this compatibility correction.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: The implementation must keep import and exact replay source
  mappings identical; the two restart-boundary tests guard against drift.
