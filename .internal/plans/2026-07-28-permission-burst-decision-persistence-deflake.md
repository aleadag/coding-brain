# Permission-Burst Decision Persistence Deflake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the parallel permission-burst regression deterministic and prevent its panic from causing unrelated environment-lock failures.

**Architecture:** Preserve the concurrent initial `ActivityStore` batch phase, then serialize only the post-inference proposal phase inside the test. Recover the two remaining runtime-test environment guards from mutex poisoning without changing production persistence code or timing.

**Tech Stack:** Rust 2024, standard-library scoped threads and channels, Cargo test and lint tooling.

## Global Constraints

- The production decision-store timeout remains 100 ms.
- The test still starts 15 permission hooks concurrently and proves one atomic initial activity batch per request.
- Every successful request still proves `Observed -> Evaluating -> Allowed -> Delivered`.
- No production interfaces, persistence ordering, or fail-closed behavior change.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Isolate the Activity-Store Burst Regression

**Files:**
- Modify and test: `src/brain/permission_hook.rs:2417-2462`

**Interfaces:**
- Consumes: `std::thread::scope`, `std::sync::mpsc::sync_channel`, the existing 15-payload barrier, and `run_with_gate_and_stores`.
- Produces: test-local release/result channels paired with scoped worker handles; each worker is released, awaited with a fresh five-second deadline, and joined before the next worker is released.

**Acceptance Criteria:**
- All 15 hooks reach inference before any hook is released.
- The initial activity lock acquisition count remains exactly 15.
- Proposal appends no longer form a concurrent burst unrelated to the regression.
- Every hook returns an allow response with empty stderr and the complete four-state lifecycle.
- The production 100 ms decision-store timeout is unchanged.

- [ ] **Step 1: Confirm the existing failure evidence**

Run:

```bash
gh run view 30336566174 --log-failed
```

Expected: the permission-burst test reports `decision store lock timed out`; the same run then reports two `HOME_ENV_LOCK` `PoisonError` failures.

- [ ] **Step 2: Run the focused test on the unchanged source**

Run:

```bash
nix develop path:. --command cargo test --lib brain::permission_hook::tests::parallel_codex_permission_burst_preserves_complete_initial_lifecycles -- --exact
```

Expected: it may pass in isolation, matching the issue evidence that the failure is scheduler-sensitive. The CI failure is the red evidence; do not weaken the assertions to manufacture a local failure.

- [ ] **Step 3: Serialize only the post-inference phase**

Replace the separate `handles` and `releases` vectors with paired workers, then release and join each pair before releasing the next:

```rust
let results = std::thread::scope(|scope| {
    let mut workers = Vec::new();
    for payload in &payloads {
        let start = Arc::clone(&start);
        let ready_tx = ready_tx.clone();
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let (result_tx, result_rx) = mpsc::channel();
        let lifecycle = &lifecycle;
        let activity = &activity;
        let config = &config;
        workers.push((
            release_tx,
            result_rx,
            scope.spawn(move || {
                start.wait();
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                run_with_gate_and_stores(
                    Cursor::new(payload),
                    &mut stdout,
                    &mut stderr,
                    Some(config),
                    BrainGateMode::Auto,
                    lifecycle,
                    Some(activity),
                    |_, _| {
                        ready_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(suggestion(RuleAction::Approve, 0.9))
                    },
                );
                result_tx.send((stdout, stderr)).unwrap();
            }),
        ));
    }
    drop(ready_tx);
    for _ in &payloads {
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let initial_lock_acquisitions = initial_lock_acquisitions.load(Ordering::SeqCst);
    let results = workers
        .into_iter()
        .map(|(release, result_rx, handle)| {
            release.send(()).unwrap();
            let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            handle.join().unwrap();
            result
        })
        .collect::<Vec<_>>();
    (initial_lock_acquisitions, results)
});
```

- [ ] **Step 4: Verify the focused regression repeatedly**

Run the following command 20 times:

```bash
nix develop path:. --command cargo test --lib brain::permission_hook::tests::parallel_codex_permission_burst_preserves_complete_initial_lifecycles -- --exact
```

Expected: all 20 invocations pass with no stderr from the hook assertions.

### Task 2: Contain Environment-Mutex Poisoning

**Files:**
- Modify and test: `src/runtime/brain.rs:831-895`

**Interfaces:**
- Consumes: `crate::config::HOME_ENV_LOCK: std::sync::Mutex<()>`.
- Produces: poison-tolerant mutex guards in the two runtime tests that mutate `HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME`.

**Acceptance Criteria:**
- Both remaining plain `HOME_ENV_LOCK.lock().unwrap()` consumers recover the inner guard from `PoisonError`.
- Environment mutation remains serialized and restored by `RefreshEnvGuard`.
- The two runtime tests pass independently and in the full suite.

- [ ] **Step 1: Confirm the two poison-sensitive consumers**

Run:

```bash
rg -n 'HOME_ENV_LOCK\.lock\(\)\.unwrap\(\)' src/runtime/brain.rs
```

Expected: matches at the start of `live_brain_refresh_reports_busy_during_activity_lock_contention` and `live_actions_reject_unknown_authority_before_discovery`.

- [ ] **Step 2: Recover each environment guard from poisoning**

Change both acquisitions to:

```rust
let _env_lock = crate::config::HOME_ENV_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
```

- [ ] **Step 3: Verify the previously collateral tests**

Run:

```bash
nix develop path:. --command cargo test --lib runtime::brain::tests::live_brain_refresh_reports_busy_during_activity_lock_contention -- --exact
nix develop path:. --command cargo test --lib runtime::brain::tests::live_actions_reject_unknown_authority_before_discovery -- --exact
```

Expected: both tests pass.

- [ ] **Step 4: Run the complete quality gates**

Run:

```bash
nix develop path:. --command cargo test --all-targets
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo fmt --check
git -c core.whitespace=trailing-space,space-before-tab diff --check
git status --short
```

Expected: tests, Clippy, formatting, and whitespace checks exit successfully; status shows only the approved spec, plan, and the two test-source files. The decision-store timeout remains `Duration::from_millis(100)`.

- [ ] **Step 5: Stop before consequential repository actions**

Report the diff, verification evidence, and Beads status. Do not commit, push, publish, or sync without explicit user authorization.
