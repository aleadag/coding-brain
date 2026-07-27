# Remove Legacy Usage and Cost Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Remove every active and dormant token-usage, cost, quota, pricing, and burn-rate surface while preserving a bounded optional context-pressure signal and all unrelated Brain behavior.

**Architecture:** First add derived context pressure alongside the legacy telemetry flow so every intermediate checkpoint compiles. Then remove peripheral public cost APIs, persistence/runtime cost fields, and analytics consumers while the old session fields still exist. Only after every consumer is neutralized does the plan delete session/monitor ledgers, raw parser fields, and pricing profiles; documentation and a production-source boundary test close the work.

**Tech Stack:** Rust 2024 workspace, Serde/serde_json, Cargo tests, Clippy, rustfmt, direnv/Nix development environment, Beads.

## Global Constraints

- Retain only a derived context-window percentage; raw token counts must not enter final retained DTOs, logs, diagnostics, or serialized output.
- Represent context pressure as bounded `Option<u8>`; `None` means unavailable and must never be interpreted as `0%`.
- Prefer provider-supplied context-window maxima, then known model mappings; unknown models without a maximum produce `None`.
- New records omit usage, cost, quota, pricing, and burn-rate fields.
- Upgraded readers accept old records; semantic downgrade to telemetry-writing binaries is unsupported.
- Discard a whole legacy preference when any condition is cost-based; never broaden it by dropping only that condition.
- Preserve status, lifecycle, permission, outcome correlation, provider recovery, navigation, and context-rot behavior.
- Remove public telemetry APIs without compatibility shims; these crates are pre-1.0.
- Add no feature flag, migration, replacement telemetry, cache, index, or freshness subsystem.
- Every task ends with its focused tests and `cargo check --workspace`.
- Do not commit, push, publish, or sync unless the user separately authorizes it. If a commit is authorized, use the repository’s emoji conventional format and include the Beads issue id.

---

## File Structure

- `crates/coding-brain-core/src/context_pressure.rs`: one overflow-safe raw-evidence-to-percentage conversion.
- `crates/coding-brain-core/src/models.rs`: known context-window lookup added first; pricing removed only after all consumers are gone.
- `crates/coding-brain-core/src/{codex_transcript,transcript}.rs`: derived pressure added alongside legacy usage fields, then raw fields removed in Task 5.
- `crates/coding-brain-core/src/{session,monitor}.rs`: derived pressure added in Task 1; ledgers removed in Task 5.
- `crates/coding-brain-core/src/{history,hooks,helpers,rules}.rs`: peripheral public cost APIs removed in Task 2.
- `src/brain/{decisions,pref_store,preferences,context,review,outcomes}.rs` and runtime/TUI DTOs: persistence/output cost removal in Task 3.
- `src/brain/{autopsy,detectors,insights,sequences,evals}.rs`: token/cost analytics removal in Task 4.
- `crates/coding-brain-core/src/health.rs`: cost/token-efficiency health logic removed with the core ledgers in Task 5.
- `tests/fixtures/legacy-*.json`: realistic legacy inputs and forbidden output-key lists, kept outside production source.
- `tests/public_namespace.rs`, current docs, `CHANGELOG.md`, and `blog/posts.md`: final boundary enforcement in Task 6.

---

### Task 1: Add Derived Context Pressure Alongside Legacy Telemetry

**Files:**

- Create: `crates/coding-brain-core/src/context_pressure.rs`
- Modify: `crates/coding-brain-core/src/lib.rs:15-40`
- Modify: `crates/coding-brain-core/src/models.rs:1-295`
- Modify: `crates/coding-brain-core/src/codex_transcript.rs:1-380`
- Modify: `crates/coding-brain-core/src/transcript.rs:1-190`
- Modify: `crates/coding-brain-core/src/session.rs:123-430`
- Modify: `crates/coding-brain-core/src/monitor.rs:1-560`
- Modify: `tests/integration_tests.rs:500-1010`

**Interfaces:**

- Consumes: provider-native raw counts and context-window maxima during parsing.
- Produces:
  - `context_pressure::percent(used: u64, capacity: u64) -> Option<u8>`;
  - `models::context_window(model: &str) -> Option<u64>`;
  - `codex_transcript::parse_timed_line_with_capacity(line: &str, fallback_capacity: Option<u64>) -> Option<TimedCodexEvent>`;
  - `CodexTokenCount::context_pressure: Option<u8>`;
  - `TranscriptMessage::context_pressure: Option<u8>`;
  - `AgentSession::context_pressure: Option<u8>`.
- Temporarily retains: all legacy usage/cost parser and session fields so this task is additive and independently green.

**Acceptance Criteria:**

- Zero capacity returns `None`; arithmetic cannot overflow; used values above capacity clamp to `Some(100)`.
- Provider capacity wins over monitor-supplied known-model capacity.
- Unknown models without provider capacity return `None`.
- Valid evidence updates session pressure; malformed later evidence does not overwrite it.
- Ordinary incremental reads retain pressure; transcript truncation/replacement and session reconstruction clear it before rescan.
- Existing telemetry behavior still compiles unchanged, ready for later removal.

- [ ] **Step 1: Add the failing bounded-conversion test**

Create `context_pressure.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::percent;

    #[test]
    fn context_pressure_is_bounded_and_optional() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(50, 100), Some(50));
        assert_eq!(percent(u64::MAX, u64::MAX), Some(100));
        assert_eq!(percent(u64::MAX, 1), Some(100));
    }
}
```

- [ ] **Step 2: Run the test and verify the expected compile failure**

Run:

```bash
direnv exec . cargo test -p coding-brain-core context_pressure_is_bounded_and_optional
```

Expected: FAIL because `percent` does not exist.

- [ ] **Step 3: Implement and export the conversion**

```rust
pub fn percent(used: u64, capacity: u64) -> Option<u8> {
    if capacity == 0 {
        return None;
    }
    let value = (u128::from(used) * 100 / u128::from(capacity)).min(100);
    Some(value as u8)
}
```

Add `pub mod context_pressure;` to core `lib.rs`.

- [ ] **Step 4: Add a known-model context-window lookup without removing pricing**

Add alongside the existing profile APIs:

```rust
pub fn context_window(model: &str) -> Option<u64> {
    match shorten_model(model).as_str() {
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => Some(1_050_000),
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" => Some(258_400),
        _ => None,
    }
}
```

Test all known families and `assert_eq!(context_window("custom-model"), None)`.
Do not remove existing pricing APIs in this task.

- [ ] **Step 5: Add pressure to Codex token-count parsing**

Keep `CodexTokenUsage` and existing `CodexTokenCount` fields, then add:

```rust
pub struct CodexTokenCount {
    pub total: CodexTokenUsage,
    pub last: CodexTokenUsage,
    pub model_context_window: Option<u64>,
    pub context_pressure: Option<u8>,
}
```

Introduce:

```rust
pub fn parse_timed_line_with_capacity(
    line: &str,
    fallback_capacity: Option<u64>,
) -> Option<TimedCodexEvent> {
    // Existing parse flow, passing fallback_capacity into token_count parsing.
}
```

Choose capacity with:

```rust
let capacity = info
    .get("model_context_window")
    .and_then(Value::as_u64)
    .or(fallback_capacity);
let used = info
    .get("last_token_usage")
    .and_then(|value| value.get("input_tokens"))
    .and_then(Value::as_u64);
let context_pressure = used
    .zip(capacity)
    .and_then(|(used, capacity)| crate::context_pressure::percent(used, capacity));
```

Make existing `parse_timed_line` delegate with `None`.

- [ ] **Step 6: Add pressure to generic transcript messages**

Keep `TranscriptUsage`, then add:

```rust
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TranscriptUsage>,
    pub context_pressure: Option<u8>,
    pub content: Vec<TranscriptBlock>,
}
```

Use saturating addition for input/cache input and `models::context_window` for
the denominator. An unknown model produces `None`.

- [ ] **Step 7: Add pressure to session/monitor additively**

Initialize:

```rust
pub context_pressure: Option<u8>,
```

Use `parse_timed_line_with_capacity(&line, models::context_window(&session.model))`.
When either parser returns `Some(pressure)`, update the session. Do not remove
any legacy accumulation or pricing code yet.

When `jsonl_offset > file_len`, clear pressure before resetting the offset.
Session construction naturally starts at `None`.

- [ ] **Step 8: Add real-file fallback and reset tests**

Using the integration suite’s existing temporary JSONL helpers, prove:

- provider maximum overrides model fallback;
- missing provider maximum uses a known-model fallback;
- unknown model remains `None`;
- malformed later evidence retains the last valid value;
- no new incremental evidence retains it;
- actual truncation clears it before rescanning;
- both Codex and generic transcript paths behave consistently.

- [ ] **Step 9: Verify the task**

First confirm exact filters:

```bash
direnv exec . cargo test -- --list | rg "context_pressure|codex_transcript|transcript"
```

Then run:

```bash
direnv exec . cargo test -p coding-brain-core context_pressure
direnv exec . cargo test -p coding-brain-core codex_transcript::tests
direnv exec . cargo test -p coding-brain-core transcript::tests
direnv exec . cargo test --test integration_tests context_pressure
direnv exec . cargo check --workspace
git diff --check
```

Expected: PASS. Pricing and old usage fields still exist intentionally at this
checkpoint. Do not commit under the conservative profile.

---

### Task 2: Remove Peripheral Core Cost APIs

**Files:**

- Delete: `crates/coding-brain-core/src/history.rs`
- Modify: `crates/coding-brain-core/src/lib.rs:15-40`
- Modify: `src/lib.rs:8-14`
- Modify: `crates/coding-brain-core/src/hooks.rs:1-250`
- Modify: `crates/coding-brain-core/src/helpers.rs:1-170`
- Modify: `crates/coding-brain-core/src/rules.rs:1-390`
- Create: `tests/fixtures/legacy-forbidden-output-keys.json`
- Modify: `tests/unit_tests.rs:70-130`

**Interfaces:**

- Consumes: existing `AgentSession`, including still-present legacy fields.
- Produces: retained hook/helper/rule APIs with no budget, token, or cost
  behavior; removes the unused public history module.

**Acceptance Criteria:**

- Budget hook events and cost/token placeholders are absent.
- Webhook JSON contains retained status/context/lifecycle data only.
- Cost aggregate helper and cost rule condition are absent.
- Rule deny precedence and permission-related matching are unchanged.
- Session history’s token/cost-only public module is removed without a shim.

- [ ] **Step 1: Add assertion-failing hook/helper/rule tests**

Use complete `RawAgentSession` constructors already present in each module.
Assert retained template variables still expand, while a template containing
legacy placeholders remains literal:

```rust
let rendered = expand_template("{pid}|{project}|{cost}|{tokens_in}", &session);
assert_eq!(rendered, "12345|my-app|{cost}|{tokens_in}");
```

Assert status webhook JSON has no keys listed in
`tests/fixtures/legacy-forbidden-output-keys.json`, and
remove/replace the cost-rule tests with deny-precedence coverage.

- [ ] **Step 2: Run the focused tests and verify assertions fail**

Confirm exact filters with `cargo test -- --list`, then run hook/helper/rule
tests. Expected: FAIL because current placeholders, JSON, and rule fields still
expose cost.

- [ ] **Step 3: Narrow hooks**

Remove `BudgetWarning`, `BudgetExceeded`, `{cost}`, `{tokens_in}`, and
`{tokens_out}`. Keep `{context_pct}` and render unavailable pressure as empty:

```rust
.replace(
    "{context_pct}",
    &session
        .context_pressure
        .map(|value| value.to_string())
        .unwrap_or_default(),
)
```

Do not change hook spawning, redaction, lifecycle events, or permission code.

- [ ] **Step 4: Narrow helpers and rules**

Remove cost/verification/profile/token keys from webhook JSON and delete
`create_aggregate_session`.

Remove `AutoRule::match_cost_above` and only its match branch/tests. Preserve
status/tool/command/project/error/conflict matching and deny precedence.

- [ ] **Step 5: Delete the public history surface**

Delete `history.rs` and remove its core/root re-exports. Do not delete generic
transcript monitoring or legacy filesystem paths.

- [ ] **Step 6: Verify the task**

```bash
direnv exec . cargo test -p coding-brain-core hooks::tests
direnv exec . cargo test -p coding-brain-core rules::tests
direnv exec . cargo test --test unit_tests
direnv exec . cargo check --workspace
git diff --check
```

Expected: PASS. Legacy session/monitor ledgers remain temporarily. Do not
commit without separate authorization.

---

### Task 3: Make Decision Persistence and Runtime DTOs Cost-Free

**Files:**

- Create: `tests/fixtures/legacy-decision-context-telemetry.json`
- Create: `tests/fixtures/legacy-preferences-cost-condition.json`
- Modify: `tests/fixtures/legacy-forbidden-output-keys.json`
- Modify: `src/brain/decisions.rs:120-810`
- Modify: `src/brain/pref_store.rs:1-110`
- Modify: `src/brain/preferences.rs:1-710`
- Modify: `src/brain/context.rs:1-90`
- Modify: `src/brain/review.rs:220-270`
- Modify: `src/brain/outcomes.rs:100-220`
- Modify: `src/commands.rs:630-680`
- Modify: `crates/coding-brain-core/src/runtime.rs:45-120`
- Modify: `src/runtime/brain.rs:950-1000`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:1840-1880`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs:1030-1080`
- Modify: `tests/brain_tui_smoke.rs:70-100`

**Interfaces:**

- Consumes: Task 1’s `AgentSession::context_pressure`; old session cost fields
  remain but are no longer read here.
- Produces: `DecisionContext::context_pct: Option<u8>`, cost-free
  `DecisionSummary`, and baseline rows containing success/sample/duration only.

**Acceptance Criteria:**

- New decisions/runtime summaries omit cost and burn rate.
- Legacy decisions retain valid non-telemetry context without requiring removed fields.
- Whole legacy cost-conditioned preferences are discarded.
- Legacy neighboring preference records survive.
- Review, prompt context, baseline CLI, runtime, and TUI fixtures contain no cost output.

- [ ] **Step 1: Create realistic compatibility/output fixtures**

`legacy-decision-context-telemetry.json` contains one old decision with
`cost_usd`, `burn_rate_per_hr`, and retained fields including
`context_pct: 80`.

`legacy-preferences-cost-condition.json` contains:

- one pattern with `cost_above` plus `no_errors`;
- one neighboring pattern with only `no_errors`.

`legacy-forbidden-output-keys.json` is:

```json
[
  "cost_usd",
  "burn_rate_per_hr",
  "tokens_in",
  "tokens_out",
  "usage_metrics_available",
  "estimate_verified",
  "profile_source"
]
```

Production/unit-test Rust loads these through `include_str!` and does not spell
the obsolete identifiers itself.

- [ ] **Step 2: Add failing pure-parser and output tests**

Extract a private `parse_decision_value` and have `read_all_decisions` call it.
Test the legacy fixture directly without changing process-global state.

Assert:

```rust
assert_eq!(decision.context.as_ref().unwrap().context_pct, Some(80));
```

Parse the preference fixture and assert only the neighboring non-cost pattern
survives. Load the output-key fixture and assert none occur in newly written
decision/runtime JSON.

- [ ] **Step 3: Run tests and verify semantic assertion failures**

Confirm filters with:

```bash
direnv exec . cargo test -- --list | rg "legacy_context|legacy_cost_condition|telemetry_free"
```

Run those exact filters. Expected: FAIL because current context parsing
requires cost fields and new output still exposes cost.

- [ ] **Step 4: Narrow decision context and parsing**

```rust
pub struct DecisionContext {
    pub context_pct: Option<u8>,
    pub last_tool_error: bool,
    pub error_message: Option<String>,
    pub model: String,
    pub elapsed_secs: u64,
    pub files_modified_count: u32,
    pub total_tool_calls: u32,
    pub has_file_conflict: bool,
    pub status: String,
    pub recent_error_count: u8,
    pub subagent_count: u8,
    pub hour: Option<u8>,
}
```

Snapshot `session.context_pressure`; omit removed keys. Parse retained fields
independently with safe existing defaults. Continue defaulting a missing
provider to Codex.

- [ ] **Step 5: Drop whole cost-conditioned preferences**

Before parsing conditions:

```rust
if raw_conditions.iter().any(|condition| {
    matches!(
        condition.get("type").and_then(serde_json::Value::as_str),
        Some("cost_below" | "cost_above")
    )
}) {
    return None;
}
```

Remove `CostBelow`/`CostAbove`, cost split selection, burn-rate temporal
patterns, and cost-condition matching. Make context split/matching skip
`None` rather than classify it as low pressure.

- [ ] **Step 6: Remove cost from runtime/review/outcomes**

Delete `DecisionSummary::cost_usd` and all initializers. Remove cost from
review and Brain session summaries.

Use:

```rust
pub struct ApproachBaselineRow {
    pub approach_ref: String,
    pub success_rate: f64,
    pub sample_count: u32,
    pub median_duration_ms: Option<u64>,
}
```

Delete cost buckets and render only `SUCC%`, `N`, `MED_MS`, and `APPROACH`.

- [ ] **Step 7: Verify the task**

```bash
direnv exec . cargo test decisions::tests
direnv exec . cargo test pref_store::tests
direnv exec . cargo test preferences::tests
direnv exec . cargo test outcomes::tests
direnv exec . cargo test -p coding-brain-tui
direnv exec . cargo test --test brain_tui_smoke
direnv exec . cargo check --workspace
git diff --check
```

Expected: PASS. Legacy session ledgers still compile but no persistence/runtime
consumer exposes them. Do not commit without authorization.

---

### Task 4: Remove Usage and Cost Analytics

**Files:**

- Create: `tests/fixtures/legacy-cost-insights.json`
- Create: `tests/fixtures/legacy-cost-sequences.json`
- Modify: `src/brain/autopsy.rs:1-900`
- Modify: `src/brain/detectors.rs:1-680`
- Modify: `src/brain/insights.rs:1-520`
- Modify: `src/brain/sequences.rs:1-570`
- Modify: `src/brain/evals.rs:1-500`
- Modify: `src/brain/context.rs:1-470`

**Interfaces:**

- Consumes: Task 3’s cost-free `DecisionContext`.
- Produces: behavioral/context analytics based on tools, errors, edits,
  outcomes, and optional derived pressure only.

**Acceptance Criteria:**

- Autopsy has no token totals, waste, cost efficiency, or token-based context-bloat analysis.
- Error cascades, repeated reads, undo/redo, tests/lint detection, and edit efficiency remain.
- Cost insight categories/detectors and cost sequence fields are absent.
- Legacy cost insight/sequence fields are ignored while retained neighboring records survive.
- Optional context blowout remains and treats `None` as unavailable.
- No replacement heuristics or score renormalization are introduced.

- [ ] **Step 1: Add failing output and compatibility tests**

Assert autopsy JSON has no `"cost"` object and text has none of the forbidden
output keys loaded from the shared fixture.

Create legacy insight/sequence fixtures containing one obsolete cost item and
one retained item. Test their pure value loaders: obsolete data is dropped or
ignored, retained data survives.

- [ ] **Step 2: Run tests and verify assertion failures**

Verify filters with `cargo test -- --list`, then run autopsy, insights, and
sequences tests. Expected: FAIL because current output and persisted shapes
still contain token/cost analytics.

- [ ] **Step 3: Simplify autopsy**

Remove:

- `FindingCategory::ContextBloat` and `CostEfficiency`;
- `AutopsyFinding::tokens_wasted`;
- `CostBreakdown`;
- cumulative token curves/fields;
- `detect_context_bloat`;
- `compute_cost_breakdown`;
- token formatting and usage parsing.

Store read/edit history as message indices only. Preserve error cascades,
repeated reads, undo/redo, quality, test/lint, and edit efficiency.

- [ ] **Step 4: Remove cost insights and sequence scoring**

Delete `InsightCategory::CostPattern`, `detect_cost_patterns`, generator/order
entries, and tests. Its legacy category parser returns `None`.

Delete `AntiPattern::avg_downstream_cost`, accumulation, JSON output, and
display suffix. Value-based legacy loaders ignore the old unknown field.

Use:

```rust
ctx.context_pct.is_some_and(|pct| pct >= 80)
```

for context terminal/blowout logic.

- [ ] **Step 5: Remove synthetic eval/context cost**

Set `session.context_pressure` directly:

```rust
session.context_pressure = u8::try_from(eval.context_pct)
    .ok()
    .map(|value| value.min(100));
```

Remove synthetic cost assignment/assertions. Keep model, status, tool, error,
and derived context information in Brain prompts.

- [ ] **Step 6: Verify the task**

```bash
direnv exec . cargo test autopsy::tests
direnv exec . cargo test detectors::tests
direnv exec . cargo test insights::tests
direnv exec . cargo test sequences::tests
direnv exec . cargo test evals::tests
direnv exec . cargo test context::tests
direnv exec . cargo check --workspace
git diff --check
```

Expected: PASS. Do not commit without authorization.

---

### Task 5: Remove Core Session, Monitor, Parser, and Pricing Ledgers

**Files:**

- Modify: `crates/coding-brain-core/src/models.rs:1-295`
- Modify: `crates/coding-brain-core/src/codex_transcript.rs:1-380`
- Modify: `crates/coding-brain-core/src/transcript.rs:1-190`
- Modify: `crates/coding-brain-core/src/session.rs:123-1050`
- Modify: `crates/coding-brain-core/src/monitor.rs:1-1100`
- Modify: `crates/coding-brain-core/src/health.rs:1-920`
- Modify: `tests/unit_tests.rs:70-130`
- Modify: `tests/integration_tests.rs:300-1450`

**Interfaces:**

- Consumes: Task 1’s derived pressure path and Tasks 2-4’s neutralized consumers.
- Produces: final cost-free parser/session/monitor types; `models.rs` contains
  normalization and known context-window lookup only.

**Acceptance Criteria:**

- Raw token usage structs/fields, session ledgers, subagent cost rollups, pricing profiles, and burn rate are absent.
- Session JSON retains `context_pct` and non-telemetry identity/status/lifecycle/tool/error data.
- Context saturation, proactive compaction, error acceleration, reread detection, status, lifecycle, tools, recovery, and subagent count remain.
- Cost-stall and token-efficiency checks are removed without replacement or score renormalization.
- Full integration suite compiles after public pre-1.0 API deletion.

- [ ] **Step 1: Add failing final-shape tests**

Using complete existing `RawAgentSession` helpers, assert session JSON contains
`context_pct` and lacks every key from the forbidden-output fixture.

Add retained health snapshots for:

- context warning/critical;
- proactive compaction;
- error acceleration;
- reread detection;
- decay score using the unchanged 40/25/15 retained weights.

No test should reference a not-yet-created helper.

- [ ] **Step 2: Run tests and verify assertion failures**

Verify filters with `cargo test -- --list`. Run final-shape session and health
tests. Expected: current JSON still exposes legacy keys or old health output.

- [ ] **Step 3: Remove raw parser and pricing types**

Remove `TranscriptUsage`, raw `CodexTokenUsage`/token totals, and all parser
fields except derived pressure. Preserve provider-native fixture parsing.

Collapse `models.rs` to `shorten_model` plus `context_window`; remove profiles,
pricing, multipliers, overrides, and fallback pricing.

- [ ] **Step 4: Simplify `AgentSession`**

Remove:

- own/subagent/total/cache token fields;
- cost totals, ledger/freeze state, verification/profile source, burn rate;
- usage-metric availability;
- token-efficiency counters/baselines;
- `SubagentRollup`, `SubagentBreakdown`, token/cost/burn formatters.

Retain:

```rust
pub context_pressure: Option<u8>,
```

and:

```rust
pub fn context_percent(&self) -> Option<u8> {
    self.context_pressure
}
```

Keep `TelemetryStatus`: it reports transcript availability, not usage/cost.
JSON retains identity, status, model, elapsed, context, errors, files, tools,
lifecycle, and worker origin.

- [ ] **Step 5: Delete monitor ledgers**

Delete accumulation, pricing, freeze/reset accounting, subagent rollups,
`finalize_usage`, `refresh_subagent_rollups`, `update_subagent_rollup`,
`price_request`, and `estimate_cost_components`.

Retain pressure ingestion/reset behavior from Task 1 and status finalization.
Set:

```rust
session.subagent_count = session.active_subagent_jsonl_paths.len();
```

Remove token-efficiency bookkeeping from tool tracking; keep file edits,
errors, and rereads.

- [ ] **Step 6: Remove cost/token health checks**

Delete stalled-by-cost and token-efficiency checks/thresholds. Context checks
use:

```rust
let pct = f64::from(session.context_percent()?);
```

Remove only the 20-point token-efficiency contribution from decay. Preserve
the 40-point context, 25-point error, and 15-point repetition contributions;
do not renormalize.

- [ ] **Step 7: Replace obsolete integration tests**

Delete pricing, token accumulation, cost formatting, subagent cost rollup, and
history tests. Preserve/add:

- status inference;
- transcript availability;
- tool/error extraction;
- subagent count;
- known/unknown context pressure;
- actual truncation reset;
- lifecycle evidence;
- permission/rule behavior;
- cognitive health retained snapshots.

- [ ] **Step 8: Verify the task**

```bash
direnv exec . cargo test -p coding-brain-core session::tests
direnv exec . cargo test -p coding-brain-core monitor::tests
direnv exec . cargo test -p coding-brain-core health::tests
direnv exec . cargo test --test integration_tests
direnv exec . cargo test --test unit_tests
direnv exec . cargo check --workspace
git diff --check
```

Expected: PASS with no downstream compile break. Do not commit without
authorization.

---

### Task 6: Enforce Documentation and Production-Source Boundaries

**Files:**

- Modify: `tests/public_namespace.rs:95-140`
- Modify: `README.md:60-75`
- Modify: `docs/index.md:35-50`
- Modify: `docs/llms.txt:1-20`
- Modify: `docs/quickstart.md:35-50`
- Modify: `docs/reference.md:110-130`
- Modify: `docs/decisions/ADR-0004-provider-aware-guards-and-terminal-actuation.md:95-110`
- Modify: `blog/posts.md:1-180`
- Modify: `CHANGELOG.md`
- Modify: remaining tests/fixtures found by final audit

**Interfaces:**

- Consumes: final cost-free production source from Tasks 1-5.
- Produces: executable source/output boundaries and truthful current docs.

**Acceptance Criteria:**

- Current docs state: “Coding Brain does not collect or display token usage or cost.”
- Docs explain that only bounded context percentage is retained.
- Old marketing/blog text does not present cost, burn rate, or a session dashboard as current.
- Changelog records the intentional pre-1.0 API removal without versioning/release work.
- All Rust under production roots, including unit tests, rejects forbidden identifiers.
- Realistic provider/legacy fixtures remain outside scanned roots.
- Full fmt/test/Clippy/build and final privacy/Git audits pass.

- [ ] **Step 1: Strengthen the failing documentation test**

```rust
let boundary = "Coding Brain does not collect or display token usage or cost.";
for (name, documentation) in documents {
    assert!(documentation.contains(boundary), "{name}");
}
```

Run the exact existing documentation test. Expected: FAIL until docs use the
new statement.

- [ ] **Step 2: Add a full production-root source guard**

Recursively scan every `.rs` file under:

- `src`;
- `crates/coding-brain-core/src`;
- `crates/coding-brain-tui/src`.

Scan full files, including `#[cfg(test)]` modules. Do not strip test sections.
The boundary test itself is under `tests/` and is outside scanned roots.

Use:

```rust
const FORBIDDEN: &[&str] = &[
    "cost_usd",
    "burn_rate_per_hr",
    "priced_total_tokens",
    "usage_metrics_available",
    "cost_estimate_unverified",
    "input_per_m",
    "output_per_m",
    "cache_read_per_m",
    "cache_write_per_m",
    "CostBelow",
    "CostAbove",
    "median_cost_usd",
    "avg_downstream_cost",
];
```

Do not forbid `input_tokens`: parser-local provider input remains allowed in
fixtures and transient parsing. Obsolete literals needed by unit tests live in
JSON fixtures under `tests/fixtures`, not in scanned Rust.

- [ ] **Step 3: Run the source guard and verify it identifies leftovers**

```bash
direnv exec . cargo test --test public_namespace production_source_has_no_usage_or_cost_surfaces
```

Expected before cleanup: FAIL with exact file/identifier evidence.

- [ ] **Step 4: Update documentation and changelog**

Use:

```text
Coding Brain does not collect or display token usage or cost.
```

and:

```text
Coding Brain may derive a bounded context-window percentage for context-rot prevention, but it does not retain the provider token counts used to derive it.
```

Correct stale current-tense blog/marketing claims. Preserve ADR historical
context while making the present boundary explicit. Add an unreleased
changelog entry for the intentional pre-1.0 API removal; do not bump versions
or publish.

- [ ] **Step 5: Clear final source/output violations**

```bash
rg -n -i "cost_usd|burn_rate|pricing|quota|token usage|tokens_(in|out)|input_per_m|output_per_m|CostBelow|CostAbove" src crates README.md docs blog/posts.md
```

Remove production behavior or rewrite current docs. Retain only explicit
historical explanation and provider-native/legacy fixtures. Do not delete
unrelated lifecycle/outcome telemetry merely because it uses the generic word.

- [ ] **Step 6: Run compatibility and boundary tests**

Verify exact filters with `cargo test -- --list`, then run:

```bash
direnv exec . cargo test --test public_namespace
direnv exec . cargo test legacy_context_ignores_removed_telemetry
direnv exec . cargo test legacy_cost_condition_discards_whole_pattern
direnv exec . cargo test context_pressure
direnv exec . cargo check --workspace
```

Expected: PASS.

- [ ] **Step 7: Run full workspace gates**

```bash
direnv exec . cargo fmt --check
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo build
```

Expected: all exit 0.

- [ ] **Step 8: Run final privacy, diff, and Git audits**

```bash
git diff --check
git status --short
rg -n "cost_usd|burn_rate_per_hr|priced_total_tokens|usage_metrics_available|cost_estimate_unverified|input_per_m|output_per_m|cache_read_per_m|cache_write_per_m|CostBelow|CostAbove|median_cost_usd|avg_downstream_cost" src crates
```

Expected:

- no forbidden production identifiers;
- no raw provider counts in retained session/runtime/persistence types;
- only scoped spec, plan, implementation, tests, and docs changes;
- no commit, push, publish, or Beads sync without authorization.

- [ ] **Step 9: Close or hand off Beads work**

Close each child only after its focused tests and workspace check pass. Close
`codexctl-iyk` only after the full gates and final audit pass.

On failure, preserve the scoped diff and update the current task bead with the
exact command/error; do not broadly revert or discard work. Rollback is a
code-only revert and retains the accepted older-binary telemetry caveat.

---

## Stress Test Results

**Reviewed:** 2026-07-24
**Verdict:** Proceed after revision; all 10 branches resolved.

- Task 1 is additive, so every task boundary compiles.
- Core removal is split into peripheral API cleanup and final ledger deletion.
- Tests use real constructors, semantic assertions, and verified Cargo filters.
- Context pressure prefers provider capacity, falls back to known models, keeps
  the last valid incremental value, and clears on truncation or reconstruction.
- Legacy compatibility uses pure value parsers and realistic fixtures.
- Deletion order keeps each of the six tasks independently green.
- Behavioral analytics remain without replacement heuristics or score
  renormalization.
- The production-source guard scans all Rust under production roots, including
  inline unit tests; obsolete literals live only in external fixtures.
- Every task runs focused tests plus a workspace check; the final task runs all
  quality, privacy, source, diff, and Git gates without committing.
- Existing Beads tasks are migrated in place; only the missing fifth
  implementation task is added and dependencies are rewired.

No work is deferred. Confidence is high because the revised sequence preserves
compilation checkpoints and explicitly tests both compatibility and removal
boundaries.
