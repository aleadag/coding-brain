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
