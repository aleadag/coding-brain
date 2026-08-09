# Research: v0.59.1 lifecycle authority correlation

> **Date:** 2026-08-05
> **Bead:** codexctl-2o9fo
> **Status:** Complete

## Summary

The v0.59.1 lifecycle snapshot retains enough exact facts to corroborate an already-imported proposal plus terminal historical row without inference. Correlation must be one-to-one, run after journal reconciliation, and preserve the migrated snapshot only after its transient permission maps are removed.

## Key Findings

### The retained snapshot contains exact correlation identifiers

> **Confidence:** high - verified directly against the v0.59.1 tag and current legacy decoder.

Each session is keyed by the injective provider/session storage key. An open session retains `current_turn`, `cwd`, `provider_session_id`, per-request disposition bits, and `permission_authorities[request_key] = { transaction_id, action }`; Antigravity additionally retains the exact request-key-to-step mapping. Snapshot validation requires authority entries to name a valid transaction and to correspond to a `Decided` request. [S1] [S2] [S3]

### Correlation can be exact but cannot independently create authority

> **Confidence:** high - the accepted storage design and schema impose this boundary.

The accepted design permits lifecycle evidence to change only the closed provenance label of an existing proposal/terminal historical row when both transaction ID and request key remain exact. The historical table fixes response eligibility false and delivery unknown, while live attempt and commit tables remain separate. [S4] [S5]

### Exact matching must include all comparable identity facts

> **Confidence:** high - this is the same identity agreement enforced for validated v0.59.1 journals.

A candidate must agree on provider, session ID, turn ID, action, provider-session ID, session cwd, and project cwd. For Codex and Claude, the request belongs to the retained `current_turn`; for Antigravity, the retained request-to-step mapping identifies `step-N` while the session remains on its validated `invocation-N` turn. No transaction ID or request key needs to be reconstructed. [S1] [S3] [S6]

## Codebase Context

- `MigrationImport::import_hook_decision` already creates only response-ineligible `proposal_terminal` rows from exact proposal/terminal agreement.
- Journal reconciliation upgrades only `proposal_terminal` rows and persists the journal's exact transaction ID and request key.
- SQLite lifecycle import intentionally calls `remove_permission_state`, so lifecycle correlation must inspect the validated legacy snapshot before those transient maps are discarded.

## Recommendations

1. Process lifecycle once after decisions and journals. This gives journal evidence precedence and avoids another legacy read.
2. Stage candidate edges in temporary SQLite tables and upgrade only one-to-one lifecycle-evidence-to-historical-row matches. Zero, multiple, or conflicting candidates remain unchanged.
3. Match all comparable retained facts: provider, session, exact provider-specific turn, action, provider-session ID, terminal session cwd, and terminal project cwd.
4. Persist only the retained transaction ID and request key, set provenance to `lifecycle_correlated`, and leave response eligibility, delivery, attempts, and commits untouched.

## Open Questions

None. The retained v0.59.1 fields support an exact conservative match without synthesizing identifiers.

## Refuted / Discarded Claims

- Matching only provider/session/turn/action is sufficient. Discarded because the retained provider-session identity and cwd are stronger comparable facts.
- Correlate lifecycle before journals. Discarded because it would make source ordering decide precedence.
- Rebuild a request key from proposal input. Discarded because the snapshot already retains the exact key and the design forbids reconstruction.

## Sources

- [v0.59.1 lifecycle projection](../../../crates/coding-brain-core/src/lifecycle/projection.rs) - Primary/codebase - 2026-08-05 - snapshot session fields, request disposition, authority, and Antigravity step mapping. [S1]
- [Provider session storage key](../../../crates/coding-brain-core/src/provider.rs) - Primary/codebase - 2026-08-05 - injective provider/session identity. [S2]
- [v0.59.1 lifecycle snapshot validation](../../../crates/coding-brain-core/src/lifecycle/store.rs) - Primary/codebase - 2026-08-05 - permission authority and provider-specific invariants. [S3]
- [Unified SQLite storage design](../specs/2026-08-04-unified-sqlite-storage-design.md) - Accepted design - 2026-08-04 - historical-only lifecycle correlation boundary. [S4]
- [Brain schema v1](../../../src/brain/storage/schema-v1/brain.sql) - Primary/codebase - 2026-08-05 - closed provenance and permanently non-live historical relation. [S5]
- [Legacy journal validation](../../../src/brain/permission_transaction.rs) - Primary/codebase - 2026-08-05 - complete comparable identity agreement. [S6]
