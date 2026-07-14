# Brain-Only Codexctl Contraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Contract codexctl to a local-brain companion that retains advisory mode and opt-in `--auto-run` while removing durable task execution and distributed coordination.

**Architecture:** Keep Codex session discovery, transcript and health analysis, deterministic rules, local inference, brain learning, the TUI, and immediate brain actions. Remove coord, loop, bus, relay, hive, external-agent delegation, and task-file execution; Beads remains an external source of durable coordination rather than a codexctl dependency.

**Tech Stack:** Rust 2024 workspace, Clap, Serde/JSONL, Ratatui/Crossterm, local Ollama or OpenAI-compatible inference endpoint, Jujutsu, Beads.

**Spec:** `.internal/specs/2026-07-14-brain-only-architecture-design.md`

**Beads epic:** `codexctl-j4o`

## Global Constraints

- `--auto-run` remains opt-in.
- Deterministic deny rules always override model output.
- The retained brain action set is `approve`, `deny`, `send`, `terminate`, `route`, and `spawn`; context-saturation restart remains lifecycle behavior.
- Preserve brain preference learning, retrieval, review, metrics, risk analysis, autopsy, briefing, outcomes, and AGENTS.md gardening.
- Low-confidence or file-conflicting suggestions stay advisory; session spawning continues to enforce `max_sessions`.
- Brain inference and terminal-action failures fall back to manual control and must not create autonomous retries.
- `route` and `spawn` remain single, session-triggered actions; they must not gain task dependencies, ownership, verifier state, or durable retry queues.
- Brain decisions, preferences, prompt overrides, mailbox data, `.codexctl.toml`, and `~/.config/codexctl/config.toml` remain compatible.
- Normal startup must not modify or delete legacy coord, bus, relay, hive, or loop data.
- `codexctl init --purge` remains the only path that deliberately deletes `~/.codexctl`.
- Do not add a Beads library, CLI wrapper, state mirror, or migration.
- Keep every implementation changeset in emoji conventional form and include its Beads task ID.
- Preserve Rust 1.88 compatibility.
- Avoid unrelated brain refactoring.

## File Structure

The final ownership boundaries are:

- `crates/codexctl-core/src/{config,runtime,rules}.rs`: brain configuration, brain/session runtime contracts, and the six immediate actions.
- `crates/codexctl-tui/src/`: session monitoring and brain-only interaction; no coord, bus, relay, or hive state.
- `src/brain/`: local inference, context, outcomes, decisions, learning, review, metrics, and mailbox delivery.
- `src/runtime/`: live implementations for sessions, brain views, brain actions, review, and mailbox delivery.
- `src/{config,doctor,init}/`: brain-focused configuration, diagnostics, hooks, onboarding, and explicit purge behavior.
- `src/{main,commands}.rs`: brain/session CLI dispatch and brain-only headless execution.

The following implementation directories and files are removed after their callers are decoupled:

- `src/coord/`
- `src/loop/`
- `src/bus/`
- `src/relay/`
- `src/hive/`
- `src/orchestrator.rs`
- `src/ingest.rs`
- `src/brain/agents.rs`
- `src/runtime/{bus,coord,hive}.rs`

---

### Task 1: Isolate Local Brain State (`codexctl-9iv`)

**Files:**

- Modify: `src/brain/context.rs`
- Modify: `src/brain/prompts.rs`
- Modify: `src/brain/client.rs`
- Modify: `src/brain/engine.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/outcomes.rs`
- Modify: `src/commands.rs`
- Test: inline `#[cfg(test)]` modules in the files above

**Interfaces:**

- Consumes: existing `DecisionRecord`, `ResolvedOutcome`, `BrainContext`, and `BrainSuggestion`.
- Produces: `brain::outcomes::ApproachBaselineRow` and `rank_approaches(&[DecisionRecord], &HashMap<String, ResolvedOutcome>, Option<&str>) -> Vec<ApproachBaselineRow>`.
- Produces: brain prompts built only from session, transcript, git, global-session, preference, and few-shot context.

**Acceptance Criteria:**

- Brain prompts contain no coord or hive context.
- Decision distillation uses only local brain stores.
- `codexctl --brain-baseline` no longer imports hive types.
- Advisory and `--auto-run` inference paths retain the six approved actions.
- Characterization tests prove advisory default behavior, confidence demotion, deny precedence, spawn limits, and inference-failure fallback.
- Route PIDs use checked conversion and must resolve to an active discovered session before delivery.
- Focused brain tests pass with both the current default build and `--no-default-features`.

- [ ] **Step 1: Start the task changeset and claim the bead**

```bash
bd update codexctl-9iv --claim
jj new -m "🧹 refactor: isolate local brain state (codexctl-9iv)"
```

Expected: `codexctl-9iv` is `in_progress` and the working copy is an empty described changeset.

- [ ] **Step 2: Add failing prompt and outcome-baseline tests**

Add to `src/brain/prompts.rs` tests:

```rust
#[test]
fn advisory_prompt_has_no_external_coordination_slots() {
    let prompt = builtin(ADVISORY);
    assert!(!prompt.contains("coordination_context"));
    assert!(!prompt.contains("hive_context"));
}
```

Add to `src/brain/client.rs` tests so all retained structured actions are covered:

```rust
#[test]
fn parse_route_suggestion() {
    let suggestion = parse_suggestion_json(
        r#"{"action":"route","target_pid":42,"reasoning":"better owner","confidence":0.9}"#,
    )
    .unwrap();
    assert_eq!(suggestion.action, RuleAction::Route { target_pid: 42 });
}

#[test]
fn parse_route_rejects_pid_overflow() {
    let json = r#"{"action":"route","target_pid":4294967296,"reasoning":"invalid"}"#;
    assert!(parse_suggestion_json(json).is_err());
}

#[test]
fn parse_spawn_suggestion() {
    let suggestion = parse_suggestion_json(
        r#"{"action":"spawn","spawn_prompt":"run tests","spawn_cwd":"/work","reasoning":"parallel","confidence":0.9}"#,
    )
    .unwrap();
    assert_eq!(
        suggestion.action,
        RuleAction::Spawn {
            prompt: "run tests".into(),
            cwd: "/work".into(),
        }
    );
}
```

Add to `src/brain/outcomes.rs` tests:

```rust
fn decision_with_id(id: &str, project: &str, tool: &str, command: &str) -> DecisionRecord {
    DecisionRecord {
        timestamp: "2026-07-14T00:00:00Z".into(),
        pid: 1,
        project: project.into(),
        tool: Some(tool.into()),
        command: Some(command.into()),
        brain_action: "approve".into(),
        brain_confidence: 0.9,
        brain_reasoning: "fixture".into(),
        user_action: "auto".into(),
        context: None,
        outcome: None,
        decision_type: crate::brain::decisions::DecisionType::Session,
        suggested_at: None,
        resolved_at: None,
        override_reason: None,
        decision_id: Some(id.into()),
        brain_decision_ms: None,
        cache_hit: None,
        canonical: None,
    }
}

fn resolved_outcomes(
    rows: &[(&str, &str, i32, u64)],
) -> std::collections::HashMap<String, ResolvedOutcome> {
    rows.iter()
        .map(|(id, project, exit_code, duration_ms)| {
            (
                (*id).to_string(),
                ResolvedOutcome {
                    decision_id: (*id).to_string(),
                    tool: "Bash".into(),
                    command: Some("cargo test".into()),
                    project: (*project).to_string(),
                    exit_code: Some(*exit_code),
                    duration_ms: Some(*duration_ms),
                    stderr_tail: None,
                    ts: 1,
                },
            )
        })
        .collect()
}

#[test]
fn rank_approaches_is_local_and_project_filterable() {
    let decisions = vec![
        decision_with_id("d1", "alpha", "Bash", "cargo test"),
        decision_with_id("d2", "alpha", "Bash", "cargo test"),
        decision_with_id("d3", "beta", "Bash", "cargo test"),
    ];
    let resolved = resolved_outcomes(&[
        ("d1", "alpha", 0, 100),
        ("d2", "alpha", 1, 300),
        ("d3", "beta", 0, 200),
    ]);

    let rows = rank_approaches(&decisions, &resolved, Some("alpha"));

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].approach_ref, "pattern:Bash:cargo test");
    assert_eq!(rows[0].sample_count, 2);
    assert_eq!(rows[0].success_rate, 0.5);
    assert_eq!(rows[0].median_duration_ms, Some(200));
}

#[test]
fn rank_approaches_breaks_score_ties_by_approach_ref() {
    let decisions = vec![
        decision_with_id("d1", "alpha", "Bash", "z command"),
        decision_with_id("d2", "alpha", "Bash", "a command"),
    ];
    let resolved = resolved_outcomes(&[("d1", "alpha", 0, 100), ("d2", "alpha", 0, 100)]);

    let rows = rank_approaches(&decisions, &resolved, Some("alpha"));

    assert_eq!(rows[0].approach_ref, "pattern:Bash:a command");
    assert_eq!(rows[1].approach_ref, "pattern:Bash:z command");
}
```

Add to `src/brain/engine.rs` tests. These tests inject results through the engine's private channel and keep the fixture session in `Processing` state so the tick does not start a real inference request:

```rust
fn suggestion(action: RuleAction, confidence: f64) -> BrainSuggestion {
    BrainSuggestion {
        action,
        message: None,
        reasoning: "fixture".into(),
        confidence,
        suggested_at: 0,
    }
}

fn inject(engine: &BrainEngine, pid: u32, suggestion: Result<BrainSuggestion, String>) {
    engine.tx.send(BrainResult { pid, suggestion }).unwrap();
}

#[test]
fn advisory_mode_queues_high_confidence_suggestion() {
    let mut engine = BrainEngine::new(make_config());
    let session = make_session(100, SessionStatus::Processing);
    inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 1.0)));

    assert!(engine.tick(&[session], &[]).is_empty());
    assert!(engine.pending.contains_key(&100));
}

#[test]
fn auto_mode_defers_low_confidence_suggestion() {
    let mut config = make_config();
    config.auto_mode = true;
    let mut engine = BrainEngine::new(config);
    let mut session = make_session(100, SessionStatus::Processing);
    session.pending_tool_name = Some("stress-test-unknown-tool".into());
    inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 0.0)));

    assert!(engine.tick(&[session], &[]).is_empty());
    assert!(engine.pending.contains_key(&100));
}

#[test]
fn deny_rule_overrides_auto_mode() {
    let mut config = make_config();
    config.auto_mode = true;
    let mut engine = BrainEngine::new(config);
    let session = make_session(100, SessionStatus::Processing);
    let deny = crate::rules::AutoRule::new("deny all".into(), RuleAction::Deny);
    inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 1.0)));

    let actions = engine.tick(&[session], &[deny]);

    assert!(actions[0].1.contains("deny rule"));
    assert!(!engine.pending.contains_key(&100));
}

#[test]
fn auto_spawn_respects_max_sessions() {
    let mut config = make_config();
    config.auto_mode = true;
    config.max_sessions = 1;
    let mut engine = BrainEngine::new(config);
    let session = make_session(100, SessionStatus::Processing);
    inject(
        &engine,
        100,
        Ok(suggestion(
            RuleAction::Spawn {
                prompt: "run tests".into(),
                cwd: "/tmp/test".into(),
            },
            1.0,
        )),
    );

    let actions = engine.tick(&[session], &[]);

    assert!(actions[0].1.contains("Spawn blocked"));
}

#[test]
fn auto_route_requires_active_target() {
    let mut config = make_config();
    config.auto_mode = true;
    let mut engine = BrainEngine::new(config);
    let session = make_session(100, SessionStatus::Processing);
    inject(
        &engine,
        100,
        Ok(suggestion(RuleAction::Route { target_pid: 999 }, 1.0)),
    );

    let actions = engine.tick(&[session], &[]);

    assert!(actions[0].1.contains("target PID 999 not found"));
}

#[test]
fn inference_failure_creates_no_pending_or_task_state() {
    let mut engine = BrainEngine::new(make_config());
    let session = make_session(100, SessionStatus::Processing);
    inject(&engine, 100, Err("endpoint unavailable".into()));

    assert!(engine.tick(&[session], &[]).is_empty());
    assert!(engine.pending.is_empty());
    assert!(engine.inflight.is_empty());
}
```

- [ ] **Step 3: Run the new tests and confirm the old coupling fails them**

```bash
cargo test -p codexctl brain::prompts::tests::advisory_prompt_has_no_external_coordination_slots
cargo test -p codexctl brain::client::tests::parse_route_suggestion
cargo test -p codexctl brain::client::tests::parse_route_rejects_pid_overflow
cargo test -p codexctl brain::client::tests::parse_spawn_suggestion
cargo test -p codexctl brain::outcomes::tests::rank_approaches_is_local_and_project_filterable
cargo test -p codexctl brain::engine::tests::advisory_mode_queues_high_confidence_suggestion
```

Expected: the route/spawn parser tests pass against retained behavior; the prompt test fails because the current template still contains coord/hive slots; the outcome test fails because `rank_approaches` does not exist.

- [ ] **Step 4: Remove external coordination fields and injection paths**

Change `BrainContext` in `src/brain/context.rs` to end with the retained fields:

```rust
pub struct BrainContext {
    pub session_summary: String,
    pub recent_transcript: String,
    pub decision_prompt: String,
    pub few_shot_examples: String,
    pub preference_summary: String,
    pub global_session_map: String,
    pub git_context: String,
}
```

Remove `{{coordination_context}}` and `{{hive_context}}` from the built-in per-session prompt in `src/brain/prompts.rs`. Remove the matching formatting branches from `format_brain_prompt` and every test fixture field in `src/brain/context.rs`.

Delete the `#[cfg(feature = "coord")]` and `#[cfg(feature = "hive")]` injection blocks from `BrainEngine::spawn_inference` in `src/brain/engine.rs`. Keep preference and few-shot loading unchanged.

Delete hive feedback, coord promotion, hive export, relay signaling, and hive compaction blocks from `src/brain/decisions.rs`. Keep global/project preference writes, anti-pattern mining, and insight generation unchanged.

In `parse_suggestion_json`, replace the unchecked route PID cast with:

```rust
let target_pid = json
    .get("target_pid")
    .and_then(|value| value.as_u64())
    .ok_or("route action requires 'target_pid' field")?;
let target_pid = u32::try_from(target_pid)
    .map_err(|_| "route action 'target_pid' exceeds u32 range")?;
```

- [ ] **Step 5: Move outcome ranking into the brain**

Add to `src/brain/outcomes.rs`:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ApproachBaselineRow {
    pub approach_ref: String,
    pub success_rate: f64,
    pub sample_count: u32,
    pub median_cost_usd: Option<f64>,
    pub median_duration_ms: Option<u64>,
}

#[derive(Default)]
struct OutcomeBucket {
    samples: u32,
    successes: u32,
    costs: Vec<f64>,
    durations_ms: Vec<u64>,
}

fn approach_ref_for(decision: &DecisionRecord) -> Option<String> {
    let tool = decision.tool.as_deref()?;
    let command = decision
        .command
        .as_deref()
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "*".into());
    Some(format!("pattern:{tool}:{command}"))
}

fn median_f64(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn median_u64(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2
    } else {
        values[middle]
    })
}

pub fn rank_approaches(
    decisions: &[DecisionRecord],
    resolved: &std::collections::HashMap<String, ResolvedOutcome>,
    project: Option<&str>,
) -> Vec<ApproachBaselineRow> {
    let mut buckets = std::collections::HashMap::<String, OutcomeBucket>::new();
    for decision in decisions {
        if project.is_some_and(|name| !decision.project.eq_ignore_ascii_case(name)) {
            continue;
        }
        let Some(decision_id) = decision.decision_id.as_deref() else {
            continue;
        };
        let Some(outcome) = resolved.get(decision_id) else {
            continue;
        };
        let Some(approach_ref) = approach_ref_for(decision) else {
            continue;
        };
        let bucket = buckets.entry(approach_ref).or_default();
        bucket.samples += 1;
        if outcome.exit_code == Some(0) {
            bucket.successes += 1;
        }
        if let Some(cost) = decision
            .context
            .as_ref()
            .map(|context| context.cost_usd)
            .filter(|cost| *cost > 0.0)
        {
            bucket.costs.push(cost);
        }
        if let Some(duration_ms) = outcome.duration_ms {
            bucket.durations_ms.push(duration_ms);
        }
    }

    let mut rows = buckets
        .into_iter()
        .map(|(approach_ref, bucket)| ApproachBaselineRow {
            approach_ref,
            success_rate: bucket.successes as f64 / bucket.samples as f64,
            sample_count: bucket.samples,
            median_cost_usd: median_f64(bucket.costs),
            median_duration_ms: median_u64(bucket.durations_ms),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let a_score = a.success_rate * f64::from(a.sample_count);
        let b_score = b.success_rate * f64::from(b.sample_count);
        b_score
            .partial_cmp(&a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.approach_ref.cmp(&b.approach_ref))
    });
    rows
}
```

Update `run_brain_baseline` in `src/commands.rs` to call `brain::outcomes::rank_approaches`, apply `--tool` and `--top`, and render the same JSON/table fields it renders today.

- [ ] **Step 6: Run focused brain verification**

```bash
cargo test -p codexctl brain::
cargo test -p codexctl --no-default-features brain::
cargo check --workspace
```

Expected: all selected tests pass; no brain source imports `crate::coord`, `crate::hive`, or `crate::relay`.

- [ ] **Step 7: Verify and close the task**

```bash
rg -n 'coordination_context|hive_context|crate::(coord|hive|relay)' src/brain src/commands.rs
jj --no-pager diff --git
jj --no-pager st
bd close codexctl-9iv --reason "Brain prompts, learning, and baseline reporting now use only local brain state; focused tests pass."
```

Expected: `rg` has no live brain coupling hits, the diff contains only Task 1 files, and `codexctl-9iv` is closed.

---

### Task 2: Reduce Runtime and TUI Contracts (`codexctl-5k7`)

**Files:**

- Modify: `crates/codexctl-core/src/runtime.rs`
- Modify: `crates/codexctl-tui/src/app.rs`
- Modify: `crates/codexctl-tui/src/demo.rs`
- Modify: `crates/codexctl-tui/src/ui/{detail,mod,skills,status_bar,table}.rs`
- Modify: `src/runtime/{actions,mod,orchestrator}.rs`
- Delete: `src/runtime/{bus,coord,hive}.rs`
- Test: inline runtime and TUI tests

**Interfaces:**

- Consumes: `SessionSource`, `BrainView`, `BrainReviewView`, `BrainDriver`, `Actions`, and `SessionSnapshot`.
- Produces: `BrainDelivery::deliver_mailbox(&[SessionSnapshot]) -> Vec<(u32, String)>`.
- Produces: `Runtime::new(sessions, brain, actions, review, delivery)` with no coord/bus/hive fields.

**Acceptance Criteria:**

- Runtime composition contains only sessions, brain, immediate actions, review, and brain mailbox delivery.
- The TUI has no coord, bus, relay, or hive state, tabs, badges, controls, or demo fixtures.
- Pending brain mailbox messages are still delivered to waiting sessions.
- `codexctl-core` and `codexctl-tui` tests pass without feature flags.

- [ ] **Step 1: Start the task changeset and claim the bead**

```bash
bd update codexctl-5k7 --claim
jj new -m "🧹 refactor: reduce runtime to brain surfaces (codexctl-5k7)"
```

- [ ] **Step 2: Add a failing brain-delivery runtime test**

Add a `BrainDelivery` fixture test to `crates/codexctl-core/src/runtime.rs`:

```rust
#[test]
fn runtime_exposes_nonempty_brain_delivery_without_coordination_views() {
    let mock = std::sync::Arc::new(MockRuntime {
        mailbox_deliveries: vec![(42, "Delivered 1 message".into())],
        ..MockRuntime::default()
    });
    let runtime = Runtime::new(
        mock.clone(),
        mock.clone(),
        mock.clone(),
        mock.clone(),
        mock.clone(),
    );

    assert_eq!(
        runtime.delivery.deliver_mailbox(&[]),
        vec![(42, "Delivered 1 message".into())]
    );
}
```

- [ ] **Step 3: Run the test and confirm the old eight-trait constructor fails**

```bash
cargo test -p codexctl-core runtime_exposes_nonempty_brain_delivery_without_coordination_views
```

Expected: compilation fails because `Runtime::new` still requires coord, bus, orchestrator, and hive arguments and has no `delivery` field.

- [ ] **Step 4: Replace legacy runtime contracts with brain delivery**

In `crates/codexctl-core/src/runtime.rs`, remove coord, bus, and hive DTOs/traits; remove `Actions::bind_bus_role`; replace `Orchestrator` with:

```rust
pub trait BrainDelivery: Send + Sync {
    fn deliver_mailbox(&self, sessions: &[SessionSnapshot]) -> Vec<(u32, String)>;
}

#[derive(Clone)]
pub struct Runtime {
    pub sessions: Arc<dyn SessionSource>,
    pub brain: Arc<dyn BrainView>,
    pub actions: Arc<dyn Actions>,
    pub review: Arc<dyn BrainReviewView>,
    pub delivery: Arc<dyn BrainDelivery>,
}
```

Update `Runtime::new`, `MockRuntime`, `MockAction`, trait implementations, and runtime tests to match. Retain `DecisionScope::Orchestration`; it describes brain cross-session decisions, not the removed task runner.

Give `MockRuntime` a `pub mailbox_deliveries: Vec<(u32, String)>` fixture field and implement `BrainDelivery` by returning its clone.

Rename `src/runtime/orchestrator.rs` to `src/runtime/delivery.rs`. Its implementation must contain only:

```rust
pub struct LiveBrainDelivery;

impl codexctl_core::runtime::BrainDelivery for LiveBrainDelivery {
    fn deliver_mailbox(&self, snapshots: &[SessionSnapshot]) -> Vec<(u32, String)> {
        let live = resolve_live(snapshots);
        crate::brain::mailbox::deliver_pending(&live)
    }
}
```

Remove `bind_bus_role` from `src/runtime/actions.rs`. Update `src/runtime/mod.rs::build_runtime` to construct the five retained adapters. Delete `src/runtime/{bus,coord,hive}.rs`.

- [ ] **Step 5: Remove legacy TUI state and controls**

In `crates/codexctl-tui/src/app.rs`, remove:

- coord leases, handoffs, interrupts, refresh methods, and session badges
- bus agent/role state and role-binding input
- relay peer state and listener commands
- hive identity, peers, invite, join, share, and Hive tab behavior
- idle-task state (`IdleConfig`, `idle_tasks_launched`, and idle task status copy)

Keep session refresh, brain driver ticks, brain accept/reject, immediate actions, and mailbox delivery. Change the refresh call to:

```rust
fn deliver_brain_mailbox(&mut self, snapshots: &[SessionSnapshot]) {
    for (_, message) in self.runtime.delivery.deliver_mailbox(snapshots) {
        codexctl_core::logger::log("MAILBOX", &message);
        self.status_msg = message;
    }
}
```

Call `self.deliver_brain_mailbox(&snapshots)` after the brain driver tick. Add to `crates/codexctl-tui/src/app.rs` tests:

```rust
#[test]
fn brain_delivery_reaches_tui_status_path() {
    let mut app = make_test_app();
    app.runtime = codexctl_core::runtime::MockRuntime {
        mailbox_deliveries: vec![(13, "Delivered 1 message to high-context".into())],
        ..Default::default()
    }
    .into_runtime();
    let snapshots = app.sessions.iter().map(snapshot_from).collect::<Vec<_>>();

    app.deliver_brain_mailbox(&snapshots);

    assert_eq!(app.status_msg, "Delivered 1 message to high-context");
}
```

Do not change mailbox persistence semantics: successful terminal input marks messages delivered; failures remain pending for a later tick. Do not add cleanup, backoff, or task-style retry state.

Remove matching UI branches and fixtures from `demo.rs`, `ui/detail.rs`, `ui/skills.rs`, `ui/status_bar.rs`, and `ui/table.rs`. The Skills view may remain for local skill discovery, but it has no Hive tab or share controls.

- [ ] **Step 6: Run core and TUI tests**

```bash
cargo test -p codexctl-core runtime::
cargo test -p codexctl-tui
cargo check --workspace
```

Expected: all tests pass with no `coord`, `bus`, `relay`, or `hive` Cargo features required by the TUI.

- [ ] **Step 7: Verify and close the task**

```bash
rg -n 'CoordView|BusView|HiveActions|bind_bus_role|coord_|bus_|relay_|hive_' crates/codexctl-core/src/runtime.rs crates/codexctl-tui/src src/runtime
jj --no-pager diff --git
jj --no-pager st
bd close codexctl-5k7 --reason "Runtime and TUI now expose only session and brain responsibilities; mailbox delivery and tests pass."
```

Expected: no live legacy runtime/TUI identifiers remain.

---

### Task 3: Simplify Configuration, Onboarding, and Doctor (`codexctl-58b`)

**Files:**

- Modify: `crates/codexctl-core/src/config.rs`
- Modify: `src/config.rs`
- Modify: `src/doctor.rs`
- Modify: `src/init/{mod,phases,state}.rs`
- Modify: `src/main.rs`
- Delete: `src/brain/agents.rs`
- Modify: `src/brain/{client,engine,mod}.rs`
- Modify: `crates/codexctl-core/src/rules.rs`
- Test: inline config, doctor, init, client, engine, and rules tests

**Interfaces:**

- Consumes: existing brain and lifecycle TOML settings.
- Produces: `is_loopback_endpoint(&str) -> bool` for diagnostics.
- Produces: `legacy_config_warnings() -> Vec<(PathBuf, ConfigWarning)>` for one-time startup disclosure.
- Produces: config validation warnings for `[relay]`, `[hive]`, `[idle]`, and `[agents.*]`.
- Removes: `IdleConfig`, `IdleTask`, `RelayConfig`, `HiveConfig`, `AgentConfig`, and `RuleAction::Delegate`.

**Acceptance Criteria:**

- Brain `orchestrate`, `orchestrate_interval`, `max_sessions`, and lifecycle auto-restart still parse.
- Removed sections no longer affect runtime behavior and produce clear warnings.
- Normal startup leaves legacy data untouched; explicit purge still removes `~/.codexctl`.
- Doctor reports only brain-relevant checks and warns for non-loopback endpoints.
- Config, doctor, init, and brain action tests pass.

- [ ] **Step 1: Start the task changeset and claim the bead**

```bash
bd update codexctl-58b --claim
jj new -m "🧹 refactor: simplify brain configuration (codexctl-58b)"
```

- [ ] **Step 2: Add failing compatibility and endpoint tests**

Add to `src/config.rs` tests:

```rust
#[test]
fn removed_sections_warn_but_brain_orchestration_still_parses() {
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(
        file,
        r#"
[brain]
orchestrate = true
orchestrate_interval = 45
max_sessions = 6

[lifecycle]
auto_restart = true
retention_days = 30

[relay]
enabled = true

[hive]
enabled = true

[idle]
enabled = true

[agents.reviewer]
model = "gpt-5"
"#,
    )
    .unwrap();
    file.flush().unwrap();
    let path = file.path().to_path_buf();

    let (warnings, has_errors) = validate_config_file(&path);
    assert!(!has_errors);
    assert!(warnings.iter().any(|w| w.message.contains("[relay] is no longer supported")));
    assert!(warnings.iter().any(|w| w.message.contains("[hive] is no longer supported")));
    assert!(warnings.iter().any(|w| w.message.contains("[idle] is no longer supported")));
    assert!(warnings.iter().any(|w| w.message.contains("[agents.reviewer] is no longer supported")));
    assert!(warnings.iter().any(|w| w.message.contains("lifecycle.retention_days is no longer supported")));
    let startup_warnings = legacy_config_warnings_for_paths(&[path.clone()]);
    assert_eq!(startup_warnings.len(), 5);

    let raw = parse_config_file(&path).unwrap();
    let mut config = Config::default();
    config.apply(raw);
    let brain = config.brain.unwrap();
    assert!(brain.orchestrate);
    assert_eq!(brain.orchestrate_interval_secs, 45);
    assert_eq!(brain.max_sessions, 6);
    assert!(config.lifecycle.auto_restart);
}
```

Add to `src/doctor.rs` tests:

```rust
#[test]
fn non_loopback_brain_endpoint_is_advisory() {
    let check = check_brain_endpoint_url("https://brain.example.com/v1/chat/completions");
    assert_eq!(check.status, CheckStatus::Advisory);
    assert!(check.message.contains("transcript context may leave this machine"));
}

#[test]
fn loopback_endpoint_detection_is_exact_and_case_insensitive() {
    assert!(is_loopback_endpoint("http://LOCALHOST:11434/api/generate"));
    assert!(is_loopback_endpoint("http://127.0.0.1:8080/v1/chat"));
    assert!(is_loopback_endpoint("http://[::1]:8080/v1/chat"));
    assert!(!is_loopback_endpoint("http://localhost.example.com/v1/chat"));
}
```

Add to `src/brain/client.rs` tests:

```rust
#[test]
fn delegate_suggestion_is_rejected() {
    let json = r#"{"action":"delegate","agent":"reviewer","delegate_prompt":"review"}"#;
    assert!(parse_suggestion_json(json).is_err());
}
```

- [ ] **Step 3: Run tests and confirm current behavior fails**

```bash
cargo test -p codexctl removed_sections_warn_but_brain_orchestration_still_parses
cargo test -p codexctl non_loopback_brain_endpoint_is_advisory
```

Expected: both tests fail because relay is currently valid and endpoint locality is not diagnosed.

- [ ] **Step 4: Reduce configuration to brain/session concerns**

Remove relay, hive, idle-task, and external-agent structs, raw structs, merge logic, printing, examples, and known-key entries from `src/config.rs` and `crates/codexctl-core/src/config.rs`. Remove `retention_days` from `LifecycleConfig` while retaining:

```rust
pub struct LifecycleConfig {
    pub auto_restart: bool,
    pub restart_threshold_pct: f64,
    pub restart_only_when_idle: bool,
}
```

Keep `[orchestrate]` file-conflict settings and `[brain]` `orchestrate`, `orchestrate_interval`, and `max_sessions` settings.

Teach `validate_config_file` that the removed section bases are legacy:

```rust
fn removed_section_message(section: &str) -> Option<&'static str> {
    match section.split('.').next().unwrap_or(section) {
        "relay" | "hive" | "idle" | "agents" => Some(
            "is no longer supported by brain-only codexctl; use Beads or an external worker for durable coordination",
        ),
        _ => None,
    }
}

fn removed_key_message(section: &str, key: &str) -> Option<&'static str> {
    match (section, key) {
        ("lifecycle", "retention_days") => Some(
            "lifecycle.retention_days is no longer supported; codexctl no longer prunes coordination state",
        ),
        _ => None,
    }
}
```

Emit one warning at each removed section header, emit the explicit removed-key warning before generic unknown-key handling, and ignore those values without creating runtime config objects.

Add one-time startup warning collection without making internal `Config::load()` calls print repeatedly:

```rust
fn legacy_config_warnings_for_paths(
    paths: &[PathBuf],
) -> Vec<(PathBuf, ConfigWarning)> {
    paths
        .iter()
        .filter(|path| path.exists())
        .flat_map(|path| {
            let (warnings, _) = validate_config_file(path);
            warnings
                .into_iter()
                .filter(|warning| warning.message.contains("no longer supported"))
                .map(|warning| (path.clone(), warning))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn legacy_config_warnings() -> Vec<(PathBuf, ConfigWarning)> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    paths.push(PathBuf::from(".codexctl.toml"));
    legacy_config_warnings_for_paths(&paths)
}
```

Immediately after the single top-level `Config::load()` in `run_main`, emit each returned warning once:

```rust
for (path, warning) in config::legacy_config_warnings() {
    eprintln!("Warning: {}:{}: {}", path.display(), warning.line, warning.message);
}
```

- [ ] **Step 5: Remove non-upstream action and onboarding surfaces**

Delete `src/brain/agents.rs`, remove it from `src/brain/mod.rs`, remove `RuleAction::Delegate` from `crates/codexctl-core/src/rules.rs`, remove delegate parsing from `src/brain/client.rs`, and remove the delegate execution arm from `src/brain/engine.rs`.

In `src/init/phases.rs` and `src/init/state.rs`, remove the bus phase, bus answer fields, bus detection, and role binding. The registry becomes:

```rust
pub fn registry() -> Vec<Box<dyn Phase>> {
    vec![
        Box::new(BudgetPhase),
        Box::new(BrainPhase),
        Box::new(PluginPhase),
        Box::new(SkillsPhase),
    ]
}
```

Remove bus flags from the `init` subcommand in `src/main.rs`. In `src/init/mod.rs`, remove `upgrade_db_migrations`; reduce `run_upgrade` to the hook-refresh and onboarding-marker steps so upgrade never opens or migrates legacy coord/bus databases. Keep `run_purge` deleting the whole `~/.codexctl` directory only after explicit confirmation; update its copy to say it removes brain data and legacy codexctl state.

- [ ] **Step 6: Reduce doctor checks and add endpoint locality**

Change `run_all_checks` to:

```rust
pub fn run_all_checks() -> Vec<Check> {
    vec![
        check_binary_on_path(),
        check_codex_hooks(),
        check_brain_endpoint(),
        check_session_discovery(),
        check_terminal_integration(),
    ]
}
```

Add `is_loopback_endpoint` using URL host strings already accepted by config (`localhost`, `127.0.0.1`, `[::1]`). `check_brain_endpoint_url` returns an Advisory for other hosts while retaining the existing reachability result in its message.

```rust
fn endpoint_host(endpoint: &str) -> Option<&str> {
    let authority = endpoint.split_once("://")?.1.split('/').next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed.split_once(']').map(|(host, _)| host);
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

pub(crate) fn is_loopback_endpoint(endpoint: &str) -> bool {
    endpoint_host(endpoint).is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1")
    })
}

fn check_brain_endpoint_url(endpoint: &str) -> Check {
    if is_loopback_endpoint(endpoint) {
        Check {
            name: "brain endpoint privacy".into(),
            status: CheckStatus::Pass,
            message: format!("{endpoint} is loopback-only"),
            fix_hint: None,
        }
    } else {
        Check {
            name: "brain endpoint privacy".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "{endpoint} is not loopback; transcript context may leave this machine"
            ),
            fix_hint: Some(
                "Use a loopback endpoint or confirm the remote endpoint's privacy policy."
                    .into(),
            ),
        }
    }
}
```

Have `check_brain_endpoint` load the configured endpoint (falling back to the current Ollama URL), return the privacy Advisory first for non-loopback hosts, and otherwise run the existing reachability probe against that endpoint.

After applying CLI brain overrides in `run_main` but before constructing the engine, emit the same non-loopback disclosure when an enabled brain is configured:

```rust
if let Some(brain) = cfg.brain.as_ref().filter(|brain| brain.enabled) {
    if !doctor::is_loopback_endpoint(&brain.endpoint) {
        eprintln!(
            "Warning: brain endpoint {} is not loopback; transcript context may leave this machine",
            brain.endpoint
        );
    }
}
```

- [ ] **Step 7: Add a legacy-data preservation fixture**

Add to `src/init/mod.rs` tests:

```rust
#[test]
fn upgrade_preserves_legacy_state() {
    use std::ffi::OsString;
    use std::sync::Mutex;

    static HOME_LOCK: Mutex<()> = Mutex::new(());
    let _guard = HOME_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let old_home: Option<OsString> = std::env::var_os("HOME");
    let home = tempfile::tempdir().unwrap();
    // SAFETY: HOME mutation is serialized by HOME_LOCK for this test module.
    unsafe { std::env::set_var("HOME", home.path()) };

    let sentinels = [
        (".codexctl/coord/coord.db", b"coord".as_slice()),
        (".codexctl/bus/bus.db", b"bus".as_slice()),
        (".codexctl/hive/store.json", b"hive".as_slice()),
        (".codexctl/relay/identity.json", b"relay".as_slice()),
        (".codexctl/loop/loop.db", b"loop".as_slice()),
    ];
    for (relative, contents) in sentinels {
        let path = home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    Config::load();
    let _ = state::EnvironmentReport::detect();
    run_upgrade().unwrap();

    for (relative, contents) in sentinels {
        assert_eq!(std::fs::read(home.path().join(relative)).unwrap(), contents);
    }

    match old_home {
        Some(value) => unsafe { std::env::set_var("HOME", value) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}
```

Do not call `run_purge` in this test. Merge this module-level HOME lock with any existing environment lock rather than introducing two independent locks.

- [ ] **Step 8: Run focused verification and close the task**

```bash
cargo test -p codexctl config::
cargo test -p codexctl doctor::
cargo test -p codexctl init::
cargo test -p codexctl brain::client::
cargo test -p codexctl brain::engine::
cargo test -p codexctl-core config::
cargo test -p codexctl-core rules::
cargo check --workspace
jj --no-pager diff --git
jj --no-pager st
bd close codexctl-58b --reason "Configuration, onboarding, and doctor are brain-only; legacy warnings and data-preservation tests pass."
```

---

### Task 4: Remove Durable and Distributed Modules (`codexctl-xrt`)

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/codexctl-tui/Cargo.toml`
- Modify: `src/{lib,main,commands}.rs`
- Modify: `src/brain/{client,prompts}.rs`
- Delete: `src/{coord,loop,bus,relay,hive}/**`
- Delete: `src/orchestrator.rs`
- Delete: `src/ingest.rs`
- Modify: CLI tests in `src/main.rs` and headless tests in `src/commands.rs`

**Interfaces:**

- Consumes: the brain-only runtime, config, init, and doctor boundaries from Tasks 1-3.
- Produces: one unconditional binary build with brain/session/TUI functionality and no subsystem Cargo features.
- Produces: brain-only `run_headless(interval, config, json)` behavior.

**Acceptance Criteria:**

- Removed commands and flags are rejected by Clap.
- Cargo exposes no coord, bus, relay, or hive features and has no Rusqlite dependency.
- Brain-only headless, advisory, and `--auto-run` entry points compile and work.
- Legacy source directories and the task-file runner are gone.
- No listener, dependency queue, verifier, ownership, or autonomous task-retry path remains.
- Binary tests pass.

- [ ] **Step 1: Start the task changeset and claim the bead**

```bash
bd update codexctl-xrt --claim
jj new -m "🔥 refactor: remove durable coordination modules (codexctl-xrt)"
```

- [ ] **Step 2: Add failing CLI absence tests**

Add to `src/main.rs` tests:

```rust
#[test]
fn removed_product_surfaces_are_rejected() {
    use clap::Parser;

    for command in ["coord", "bus", "relay", "hive", "supervisor", "loop", "ingest"] {
        assert!(Cli::try_parse_from(["codexctl", command]).is_err(), "{command}");
    }

    assert!(Cli::try_parse_from(["codexctl", "--run", "tasks.json"]).is_err());
    assert!(Cli::try_parse_from(["codexctl", "--parallel"]).is_err());
    assert!(Cli::try_parse_from(["codexctl", "--decompose", "split this work"]).is_err());

    let advisory = Cli::try_parse_from(["codexctl"]).unwrap();
    assert!(!advisory.auto_run);
    let automatic = Cli::try_parse_from(["codexctl", "--auto-run"]).unwrap();
    assert!(automatic.auto_run);
}

#[test]
fn generated_help_keeps_auto_run_and_omits_removed_surfaces() {
    use clap::CommandFactory;

    let help = Cli::command().render_long_help().to_string();
    assert!(help.contains("--auto-run"));
    for command in ["coord", "bus", "relay", "hive", "supervisor", "loop", "ingest"] {
        assert!(
            !help.lines().any(|line| {
                line.trim_start()
                    .strip_prefix(command)
                    .is_some_and(|rest| {
                        rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')
                    })
            }),
            "{command} remains in generated help"
        );
    }
    for flag in ["--run", "--parallel", "--decompose"] {
        assert!(!help.contains(flag), "{flag} remains in generated help");
    }
}
```

Add a headless regression test in `src/commands.rs` around a single extracted tick helper:

```rust
#[test]
fn headless_tick_emits_brain_state_without_coordination_events() {
    let mut app = App::new();
    app.status_msg = "Brain: approved Bash".into();
    let events = headless_tick_events(&app, &std::collections::HashMap::new());

    assert!(events.iter().any(|event| event.kind == "action"));
    assert!(events.iter().all(|event| {
        !matches!(event.kind, "supervisor_tick" | "coord_summary" | "loop_outcome")
    }));
}
```

- [ ] **Step 3: Run the tests and confirm old surfaces still parse**

```bash
cargo test -p codexctl removed_product_surfaces_are_rejected
cargo test -p codexctl headless_tick_emits_brain_state_without_coordination_events
```

Expected: CLI absence fails for currently supported commands/flags; the headless helper does not exist.

- [ ] **Step 4: Remove CLI and module wiring**

In `src/main.rs` and `src/lib.rs`, remove module declarations and command variants for coord, bus, relay, hive, ingest, supervisor, and loop. Remove `--run`, `--parallel`, and `--decompose` plus their dispatch branches. Remove demo-only relay/hive initialization.

In `src/brain/client.rs`, delete `DecompositionResult`, its task DTO, `parse_decomposition_json`, and their tests. In `src/brain/prompts.rs`, delete `DECOMPOSITION`, `DECOMPOSITION_PROMPT`, and the decomposition entry from `list_prompts`; keep the advisory, orchestration, summarize, and autopsy prompts.

In `src/commands.rs`, keep the headless session/brain loop but delete supervisor sensors, actuator side effects, verifier backends, loop reconciliation, coord summaries, coord pruning, and coord interrupt creation. Add this pure event collector and emit each returned event from the loop after `app.tick()`; inference/action execution remains owned by the existing brain driver and TUI app:

```rust
#[derive(Debug)]
struct HeadlessEvent {
    kind: &'static str,
    data: serde_json::Value,
}

fn headless_tick_events(
    app: &App,
    previous: &std::collections::HashMap<u32, crate::session::SessionStatus>,
) -> Vec<HeadlessEvent> {
    let mut events = app
        .sessions
        .iter()
        .filter(|session| previous.get(&session.pid).is_none_or(|old| *old != session.status))
        .map(|session| HeadlessEvent {
            kind: "status_change",
            data: serde_json::json!({
                "pid": session.pid,
                "project": session.display_name(),
                "old_status": previous.get(&session.pid).map(ToString::to_string),
                "new_status": session.status.to_string(),
                "cost_usd": session.cost_usd,
                "context_pct": session.context_percent(),
                "decay_score": session.decay_score,
            }),
        })
        .collect::<Vec<_>>();

    if !app.status_msg.is_empty()
        && (app.status_msg.starts_with("Brain:") || app.status_msg.starts_with("MAILBOX"))
    {
        events.push(HeadlessEvent {
            kind: "action",
            data: serde_json::json!({"detail": app.status_msg}),
        });
    }
    events
}
```

Replace the existing inline status/action event branches with:

```rust
for event in headless_tick_events(&app, &prev_statuses) {
    emit_headless_event(event.kind, event.data, json_mode);
}
```

- [ ] **Step 5: Remove Cargo features and source trees**

Delete the root `[features]` table. Replace the root dependency lists with:

```toml
[dependencies]
codexctl-core = { path = "crates/codexctl-core", version = "0.52.1" }
codexctl-tui = { path = "crates/codexctl-tui", version = "0.52.1" }
ratatui = "0.29"
crossterm = "0.28"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
clap_complete = "4"
clap_mangen = "0.2"
libc = "0.2"
ctrlc = "3"

[dev-dependencies]
tempfile = "3"
```

Delete the `[features]` table from `crates/codexctl-tui/Cargo.toml`; its dependency list otherwise remains unchanged. Regenerate `Cargo.lock` through the build in Step 6 so `rusqlite`, `rmcp`, `tokio`, `schemars`, `async-trait`, and their now-unreachable transitive packages disappear.

Delete:

```text
src/coord/
src/loop/
src/bus/
src/relay/
src/hive/
src/orchestrator.rs
src/ingest.rs
```

Use `apply_patch` for tracked-file deletion. Do not delete anything under `~/.codexctl`.

- [ ] **Step 6: Build and run binary tests**

```bash
cargo build --workspace
cargo test -p codexctl removed_product_surfaces_are_rejected
cargo test -p codexctl headless_tick_emits_brain_state_without_coordination_events
cargo test --workspace
```

Expected: the workspace builds without feature flags or SQLite; all tests pass.

- [ ] **Step 7: Verify the source boundary and close the task**

```bash
rg -n 'feature = "(coord|bus|relay|hive)"|crate::(coord|bus|relay|hive)|mod (coord|bus|relay|hive)|rusqlite|DecompositionResult|DECOMPOSITION_PROMPT|parse_decomposition_json|TaskFile|TcpListener|UdpSocket' Cargo.toml crates src
jj --no-pager diff --git
jj --no-pager st
bd close codexctl-xrt --reason "Durable/distributed modules, commands, features, and dependencies are removed; brain-only workspace tests pass."
```

Expected: `rg` returns no live-source hits.

---

### Task 5: Document the Brain-Only Product (`codexctl-po3`)

**Files:**

- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `LAUNCH_POSTS.md`
- Modify: `justfile`
- Modify: `mkdocs.yml`
- Modify: `scripts/record-demos.sh`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/codexctl-core/Cargo.toml`
- Modify: `crates/codexctl-tui/Cargo.toml`
- Modify: `.github/workflows/{ci,release}.yml`
- Modify: `docs/{index,quickstart,configuration,reference,troubleshooting,contributing,terminal-support}.md`
- Modify: `docs/llms.txt`
- Delete or replace: `docs/{AGENT_BUS,agent-bus,hive-storage,relay-and-hive,relay-discovery,relay}.md`
- Delete: `docs/index-old.html`
- Delete: `docs/superpowers/`
- Delete: `.codex/skills/loop-triage/`
- Modify: generated/help documentation inputs as required by `clap_mangen`

**Interfaces:**

- Consumes: the final CLI and configuration behavior from Task 4.
- Produces: public brain-only product documentation and breaking-release guidance.

**Acceptance Criteria:**

- Public docs contain no supported coord/supervisor/loop/bus/relay/hive instructions.
- Retained brain, learning, mailbox, and `--auto-run` behavior is documented.
- The breaking contraction and legacy-data behavior are prominent.
- Package versions signal a breaking pre-1.0 release, and release workflows point at the actual `codexctl-*` crate paths.
- Beads is described as an external coordination option, not a dependency.
- Package and workflow metadata use no removed feature flags.
- Repo instructions, recipes, docs navigation, demo scripts, local skills, and launch copy expose no removed product surface.

- [ ] **Step 1: Start the task changeset, claim the bead, and load release-doc guidance**

```bash
bd update codexctl-po3 --claim
jj new -m "📝 docs: refocus codexctl on the local brain (codexctl-po3)"
```

Invoke `beads-superpowers:document-release` before editing public docs.

- [ ] **Step 2: Capture the failing documentation residual scan**

```bash
rg -n 'coord|supervisor|agent bus|\bbus\b|relay|hive|loop runtime|--run|--parallel|--decompose' \
  README.md CHANGELOG.md AGENTS.md CLAUDE.md LAUNCH_POSTS.md justfile mkdocs.yml \
  scripts .codex docs .github Cargo.toml crates/codexctl-tui/Cargo.toml
```

Expected: many live-product references remain.

- [ ] **Step 3: Rewrite the public product entry points**

Lead `README.md` and `docs/index.md` with this product contract:

```markdown
codexctl is a local-brain companion for Codex sessions. It observes active
sessions, evaluates pending actions with deterministic rules and a local LLM,
learns from operator corrections, and can execute high-confidence decisions
when `--auto-run` is enabled.
```

Document the six immediate actions, advisory versus auto-run mode, local brain data paths, endpoint privacy warning, session mailbox, and learning/review workflow. Describe Beads in one focused section as an optional external tracker for durable tasks, dependencies, claims, blockers, gates, and handoffs.

Remove obsolete user-facing subsystem docs, `docs/index-old.html`, the full `docs/superpowers/` tree, and `.codex/skills/loop-triage/`.

Update `AGENTS.md` and `CLAUDE.md` with the same brain-only architecture summary. Remove supervisor recipes from `justfile`, remove obsolete Agent Bus/Relay/Hive navigation from `mkdocs.yml`, make the recorded skills demo skills-only, and remove durable-task claims from `LAUNCH_POSTS.md`.

- [ ] **Step 4: Update compatibility and release documentation**

Add `## [0.58.0] - 2026-07-15` as a breaking entry in `CHANGELOG.md` that states:

```markdown
- Removed coord, supervisor, loop, bus, relay, hive, prompt decomposition, and
  dependency-ordered task execution to restore codexctl's local-brain focus.
- Existing legacy data is left untouched during upgrade. `codexctl init
  --purge` remains the explicit destructive cleanup path.
- Use Beads or another external workflow system when durable project
  coordination is required.
- Because legacy state and brain data schemas are preserved, users can roll
  back to `0.57.2`; back up `~/.codexctl` first as for any downgrade.
```

Update configuration/reference/troubleshooting docs with warnings for removed sections and the non-loopback endpoint disclosure.

- [ ] **Step 5: Update package and workflow metadata**

Change root and TUI package descriptions/keywords so they do not claim durable orchestration or coord/bus support. Apply these pre-1.0 breaking-version updates:

```text
Cargo.toml:                         0.57.2 -> 0.58.0
crates/codexctl-core/Cargo.toml:    0.52.1 -> 0.53.0
crates/codexctl-tui/Cargo.toml:     0.52.1 -> 0.53.0
Cargo.toml dependency pins:         codexctl-core/codexctl-tui -> 0.53.0
```

Regenerate `Cargo.lock`. Remove feature-matrix and release build flags for coord/bus/relay/hive from `.github/workflows/ci.yml` and `.github/workflows/release.yml`. Replace stale `crates/claudectl-core` and `crates/claudectl-tui` paths in the release workflow with `crates/codexctl-core` and `crates/codexctl-tui`.

- [ ] **Step 6: Run doc and metadata checks**

```bash
rg -n 'codexctl (coord|bus|relay|hive|supervisor|loop|ingest)|--features.*(coord|bus|relay|hive)|--run|--parallel|--decompose' \
  README.md AGENTS.md CLAUDE.md LAUNCH_POSTS.md justfile mkdocs.yml scripts .codex docs \
  .github Cargo.toml crates/codexctl-tui/Cargo.toml
cargo run -- --help >/tmp/codexctl-help.txt
cargo run -- man >/tmp/codexctl.1
rg -q -- '--auto-run' /tmp/codexctl-help.txt /tmp/codexctl.1
cargo metadata --no-deps --format-version 1
```

Expected: residual hits are limited to explicit removal/legacy-warning text; help, manpage generation, and Cargo metadata succeed.

- [ ] **Step 7: Verify and close the task**

```bash
jj --no-pager diff --git
jj --no-pager st
bd close codexctl-po3 --reason "Public docs, changelog, package metadata, and workflows now describe and build the brain-only product."
```

---

### Task 6: Run Residual and Release Verification (`codexctl-bna`)

**Files:**

- Modify only contraction-related files when a verification failure exposes a missed dependency or stale public surface.
- Test: full workspace and CLI.

**Interfaces:**

- Consumes: all deliverables from Tasks 1-5.
- Produces: a verified brain-only release candidate with no hidden durable coordination path.

**Acceptance Criteria:**

- No live source or public docs retain removed product surfaces except explicit legacy-warning/purge text and historical specs.
- Legacy data fixtures remain unchanged on normal startup.
- Advisory mode and `--auto-run` tests pass.
- `cargo build`, `cargo test`, `cargo fmt --check`, and `cargo clippy -- -D warnings` pass for the workspace.

- [ ] **Step 1: Start the verification changeset and claim the bead**

```bash
bd update codexctl-bna --claim
jj new -m "✅ test: verify brain-only contraction (codexctl-bna)"
```

- [ ] **Step 2: Run live-source residual scans**

```bash
rg -n 'feature = "(coord|bus|relay|hive)"|crate::(coord|bus|relay|hive)|mod (coord|bus|relay|hive)|rusqlite|RuleAction::Delegate|IdleTask|TaskFile|DecompositionResult|DECOMPOSITION_PROMPT|parse_decomposition_json|TcpListener|UdpSocket' \
  Cargo.toml crates src

rg -n 'codexctl (coord|bus|relay|hive|supervisor|loop|ingest)|--features.*(coord|bus|relay|hive)|--run|--parallel|--decompose' \
  README.md AGENTS.md CLAUDE.md LAUNCH_POSTS.md justfile mkdocs.yml scripts .codex docs .github
```

Expected: source scan has no hits; repo-owned surface scan has no operational references to removed features. Explicit compatibility/removal prose may still name the old subsystems without showing runnable commands.

- [ ] **Step 3: Run focused behavior and compatibility tests**

```bash
cargo test -p codexctl brain::
cargo test -p codexctl config::
cargo test -p codexctl doctor::
cargo test -p codexctl init::
cargo test -p codexctl-core runtime::
cargo test -p codexctl-core rules::
cargo test -p codexctl-core config::
cargo test -p codexctl-tui
cargo test -p codexctl removed_product_surfaces_are_rejected
cargo test -p codexctl generated_help_keeps_auto_run_and_omits_removed_surfaces
```

Expected: all focused suites pass, including advisory/auto-run, mailbox delivery, legacy warnings, and data preservation.

- [ ] **Step 4: Run the full project gates**

```bash
cargo fmt --check
cargo build --workspace
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: every command exits 0 with no warnings promoted by Clippy.

- [ ] **Step 5: Inspect the final jj stack**

```bash
jj --no-pager st
jj --no-pager log -r 'trunk()..@' -T 'change_id.short() ++ " " ++ description.first_line() ++ "\n"'
jj --no-pager diff --git --from 'trunk()' --to '@'
```

Expected: each implementation slice has one emoji conventional description with its Beads task ID; no unrelated changes are present.

- [ ] **Step 6: Close the verification task and epic with evidence**

```bash
bd close codexctl-bna --reason "Residual scans and focused behavior checks passed; cargo fmt, build, test, and clippy all exit 0."
bd close codexctl-j4o --reason "Brain-only contraction implemented and verified across source, CLI, compatibility, docs, and full workspace gates."
```

Do not close either bead unless all commands in Steps 2-4 have passed in the current checkout.

---

## Execution Order

The Beads graph enforces this sequence:

```text
codexctl-9iv
  -> codexctl-5k7
  -> codexctl-58b
  -> codexctl-xrt
  -> codexctl-po3
  -> codexctl-bna
```

Each task begins from the passing changeset produced by its predecessor. Do not parallelize these tasks: they deliberately remove callers before deleting the modules they consume.

## Stress Test Results: Brain-Only Contraction Plan

### Resolved Decisions

- Architecture boundary: retain `route` and `spawn` only as bounded, session-triggered actions; durable coordination remains external.
- Task sequencing: keep the six-task dependency chain and require every intermediate changeset to build.
- Behavior preservation: characterize advisory mode, auto-run confidence gates, deny precedence, spawn limits, active route targets, and inference failure.
- Upgrade and rollback: warn once during normal startup, never open legacy databases during upgrade, and preserve rollback data.
- Runtime delivery: prove a non-empty mailbox delivery crosses the runtime/TUI seam without adding task-style retries.
- Scale: preserve the existing on-demand in-memory baseline complexity and deterministic ranking; add no new cache or database.
- Release compatibility: use pre-1.0 minor version bumps and repair stale release-workflow crate paths.
- Testing: make intermediate workspace checks, CLI-help assertions, targeted residual scans, and all-target release gates deterministic.
- Security and privacy: retain opt-in auto-run, checked route PIDs, active-target and session-limit guards, deny/file-conflict precedence, startup endpoint disclosure, purge confirmation, and no listener paths.
- Repository-owned surfaces: remove obsolete coordination guidance and tooling, including the full `docs/superpowers/` tree, and rewrite the remaining contributor, navigation, demo, and launch surfaces around the brain-only product.

### Changes Made

- Added characterization and security tests for retained brain actions and auto-run gates.
- Added `cargo check --workspace` to Tasks 1-3.
- Strengthened mailbox runtime/TUI delivery fixtures.
- Added one-time startup warnings for legacy configuration and non-loopback brain endpoints.
- Added deterministic ranking parity coverage without expanding persistence.
- Added explicit `0.58.0` / `0.53.0` versioning, release-path repair, rollback guidance, and generated-help checks.
- Tightened final source, networking, documentation, CLI, and all-target verification gates.
- Added repository-wide cleanup for obsolete docs, recipes, navigation, demos, launch copy, and the local loop-triage skill; `docs/superpowers/` is deleted rather than preserved as historical documentation.

### Deferred / Parking Lot

- Optimize baseline aggregation only if measured decision-log growth makes the existing on-demand scan a real bottleneck.
- Do not add mailbox cleanup, backoff, indexing, Beads integration, or other state machinery during this contraction.

### Confidence Assessment

- Overall: High
- Areas of concern: the deletion spans multiple crates and workflows, so each task must keep its workspace check and the final residual scans must be treated as hard gates.
