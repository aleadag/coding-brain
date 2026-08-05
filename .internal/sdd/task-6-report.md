# Task 6 implementation report

Base: `e6d2fe2cbf458a58470c5b57bae3e2dc0a05aabb`

## RED evidence

- Frozen schema test: `cargo test --test sqlite_storage permission_attempt_schema_represents_pre_inference_identity_without_fabricated_authority -- --exact --nocapture` exited 101 at `tests/sqlite_storage.rs:3408`: `missing request_identity_key`.
- API seam: `cargo check --lib` exited 101 with E0583 at `src/brain/storage/mod.rs:6`: `file not found for module permissions`.
- Compatibility audit after the schema amendment initially failed three focused permission tests because old fixtures omitted `request_identity_key`; after fixture correction, the learning test exposed its stale query of removed duplicated `permission_commits.provider` columns. Both failures were corrected through normalized attempt joins.

## GREEN evidence

- Focused permission API/process suite: 13 passed, 1 intentional subprocess helper ignored, 0 failed. It covers separate-process same-request admission, sequential distinct attempts, independent request shards, optional tool evidence, atomic authority, database-inode capability binding, typed-attempt revalidation, retained absolute deadlines, invalid request keys and timestamps, pre/post-commit uncertainty, abrupt post-commit process death, delivery transaction failure, and absence of permission journals.
- Full SQLite storage suite: 102 passed, 1 intentional subprocess helper ignored, 0 failed. It includes the normalized authority constraints, unanchored-audit non-authority rule, exact bounded partial-index query plan, and frozen production/fixture schema equality.
- Full workspace/all-target suite: exit 0. The main binary library reported 1113 passed and 5 ignored; all remaining unit and integration targets completed with 0 failures.
- Quality gates: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo build --workspace --all-targets` all exited 0 under `nix develop path:.`.
- Structural audits: production and fixture schemas are byte-identical; the diff has no trailing-whitespace errors; storage keeps SQLite attachments disabled; the production provider hook contains no `BrainDb` or `admit_permission` cutover; and `coding-brain-core` has no SQLite dependency.

## Schema correction

Pre-activation schema v1 now stores the complete typed attempt identity, honest nullable provider-session/tool-use evidence, and a null action until decision commit. Authoritative decision/activity/commit relations use a non-null attempt/action anchor. Unanchored proposal and terminal rows remain non-authoritative audit evidence. `permission_commits.transaction_id` stores the exact bounded authority identifier.

## Runtime boundary

The production provider hook remains on `PermissionTransactionJournal`; Task 6 adds only inactive SQLite APIs and tests. No `ATTACH` path or core SQLite dependency is introduced.

## Independent-review follow-up

- RED: a fresh-open corruption regression failed because `permission_state` accepted a valid-length but incorrect `request_identity_key`; the no-tool regression failed with `InvalidStorage("permission identity is incomplete")` from `decision_identity`.
- Fresh authority validation now selects the complete stored attempt identity, recomputes its canonical request identity, bounds the terminal payload before deserialization, and requires the attempt update, decision, terminal event, and commit timestamps to agree in order and range. The shared validation path makes `permission_state`, `permission_decision`, and `record_delivery` fail closed on each deliberate corruption.
- `DecisionIdentity::Permission` now preserves `tool_use_id: Option<String>` through identity materialization and source-event validation. A committed no-tool permission round-trips through `decision_identity`, `decision_payload`, and paged learning reads.
- Focused follow-up evidence: 15 permission tests passed with 1 intentional helper ignored; 102 SQLite storage tests passed with 1 intentional helper ignored; both suites had 0 failures.
- Fresh follow-up gates: the full workspace/all-target suite exited 0, including 1115 passed and 5 ignored in the main binary unit target; formatting, clippy with warnings denied, and the all-target build also exited 0.

## Second independent-review follow-up

- RED: fresh authority reads accepted a terminal row whose typed `event_kind` had been changed to the valid `diagnostic` domain. The held-writer delivery fixture also returned after 1.492 seconds despite retaining a one-second absolute deadline, proving the earlier relative busy timeout was reused after validation work.
- The authority validator now resolves the anchored terminal by its source cursor, validates the global high-water, and reuses the complete bounded activity-row materializer. Kind, cursor range/high-water, every typed terminal column, payload, attempt anchor, and action anchor must therefore agree before state, decision, or delivery APIs expose authority.
- `record_delivery` reapplies the retained absolute deadline immediately before `BEGIN IMMEDIATE`. The separate-process held-writer regression returns `Busy` within the original deadline and leaves authority pending with no delivery activity.
- Process tests now use a dedicated modeled provider-response file rather than making vacuous assertions about libtest stdout redirected to null. Faults before a successful commit leave that sink absent; a successful commit writes exact bytes before delivery recording; an unwritable sink records `DeliveryFailed`; and an uncertain delivery transaction retains the written response with delivery unknown.
- Adversarial coverage now includes deterministic-safety deny/non-response semantics, invalid deterministic allow rejection, more than 16 MiB of unrelated history with an indexed permission lookup, and both Brain-to-Review and Review-to-Brain failure isolation.
- Focused evidence: 22 permission tests passed with 1 intentional helper ignored, and 102 SQLite storage tests passed with 1 intentional helper ignored; both suites had 0 failures.
- Fresh final gates: the full workspace/all-target suite exited 0, including 1087 passed with 5 ignored in the core library target and 1122 passed with 5 ignored in the main binary library target; every integration target also reported 0 failures. Formatting, clippy with warnings denied, and the all-target build each exited 0 under `nix develop path:.`.
