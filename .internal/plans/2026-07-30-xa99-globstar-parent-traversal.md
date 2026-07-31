# XA99 Globstar Parent Traversal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Deny split-capable recursive-delete targets whose active globstar and parent traversal can normalize to `/`, trusted HOME, or its ancestors, without denying quoted or provably unrelated controls.

**Architecture:** Replace the boolean HOME-pattern helper with one authoritative,
bounded symbolic component matcher. It preserves direct-pattern behavior as one
reachability path, adds explicit state exploration for `**` plus `..`, returns
tri-state reachability with semantic match provenance, and shares state and
stored-component budgets across the complete shell-safety evaluation.

**Tech Stack:** Rust, existing shell analyzer and safety evaluator, Cargo unit and process integration tests.

## Global Constraints

- Modify only `src/brain/safety.rs`, `tests/hook_activity.rs`, and these xa99 internal design/plan artifacts.
- Do not change shell parsing, assignment resolution, provider payloads, helper protocols, configuration, public documentation, dependencies, or versions.
- Use no cwd inference, filesystem reads, canonicalization, or model output in deterministic safety matching.
- Preserve existing rule IDs and existing conservative multi-globstar matches.
- Do not commit, push, publish, merge, or close `codexctl-xa99` without separate authorization.

---

### Task 1: Match globstar parent traversal within a bounded fail-closed state space

**Files:**
- Modify: `src/brain/safety.rs`
- Modify: `tests/hook_activity.rs`

**Interfaces:**
- Consumes: `ResolvedField`, `pattern_may_match_literal`, trusted `HOME`, and the existing `split_target_may_reach_home_or_ancestor` call site.
- Produces: private `PatternReachability`, `PatternMatchKind`,
  `SplitTargetRisk`, `PatternComponent`, `ResolvedPatternComponent`, and
  `PatternMatchBudget` types plus one authoritative tri-state reachability
  helper.

**Acceptance Criteria:**
- Root-reachable active `**/..` patterns deny as
  `irreversible-root-delete` before model inference.
- Zero- and nonzero-consumption `**/..` patterns that can normalize to HOME or a non-root HOME ancestor deny before model inference.
- Chained traversal, multiple globstars, and ordinary component patterns before `..` fail closed when they may reach HOME.
- Matcher state and stored-component exhaustion are bounded across the entire
  safety evaluation and deny as `unsafe-recursive-delete-expansion`.
- A quoted target expansion and an anchored unrelated active pattern remain model-backed across Codex, Claude, and Antigravity.
- Focused regressions, the prior safety corpus, and all repository quality gates pass.

**Reviewer-fix amendment (2026-07-31):**

- Add unit and all-provider RED cases for
  `/xa99-unrelated/**/..` and multiple-globstar coverage with
  `/xa99-unrelated/**/**/..`.
- Query the authoritative matcher against the empty absolute candidate before
  exact HOME and non-root HOME ancestors.
- Map traversal-proven root reachability to `irreversible-root-delete`; map
  conservative direct reachability and root `Unknown` to
  `unsafe-recursive-delete-expansion`.
- Cover `/**/xa99-unrelated/**` in unit and all-provider tests so the
  conservative direct matcher cannot overstate root provenance.
- Reuse one deliberately small state budget and one deliberately small
  component budget across consecutive helper calls. The first call must be
  reachable and the second must exhaust the shared budget as `Unknown`.
- Exercise the production aggregate budget with 500 individually unrelated
  `/xa99z*/**/**/**/zz` fields in one recursive-delete evaluation. Assert
  `unsafe-recursive-delete-expansion` and zero model requests for Codex, Claude,
  and Antigravity.

- [ ] **Step 1: Add RED unit regressions**

Extend `home_alias_classification_respects_field_splitting` in
`src/brain/safety.rs` after the existing `globstar_ancestor` assertion:

```rust
let globstar_parent_zero = format!(
    "shopt -s globstar; IFS=:; X='{home}/**/../{home_name}'; rm -rf $X"
);
let globstar_parent_nonzero = format!(
    "shopt -s globstar; IFS=:; X='{}/**/../{home_name}'; rm -rf $X",
    home_parent.display()
);
let globstar_parent_chained = format!(
    "shopt -s globstar; IFS=:; X='{home}/**/../../{home_root_component}/{home_name}'; rm -rf $X"
);
let wildcard_parent =
    format!("IFS=:; X='{home_parent}/{home_name}*/../{home_name}'; rm -rf $X");

let confirmed_bypass =
    evaluate_command(&globstar_parent_zero).unwrap_or_else(|| panic!("{globstar_parent_zero}"));
assert_eq!(
    confirmed_bypass.rule_id,
    "irreversible-home-delete",
    "{globstar_parent_zero}"
);
for command in [
    globstar_parent_nonzero,
    globstar_parent_chained,
    wildcard_parent,
] {
    assert!(evaluate_command(&command).is_some(), "{command}");
}

let quoted_globstar_parent =
    format!("X='{home}/**/../{home_name}'; rm -rf \"$X\"");
assert!(
    evaluate_command(&quoted_globstar_parent).is_none(),
    "{quoted_globstar_parent}"
);

let unrelated_globstar_parent =
    "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/../safe'; rm -rf $X";
assert!(
    evaluate_command(unrelated_globstar_parent).is_none(),
    "{unrelated_globstar_parent}"
);
```

Add a focused budget test beside the HOME classification test:

```rust
#[test]
fn pathname_parent_match_budgets_are_shared_and_fail_closed() {
    let patterns = vec![
        PatternComponent::Literal("base".into()),
        PatternComponent::Parent,
        PatternComponent::Literal("safe".into()),
    ];
    let mut state_budget = PatternMatchBudget {
        remaining_states: 5,
        remaining_components: MAX_PATTERN_MATCH_COMPONENTS,
    };

    assert_eq!(
        pattern_components_may_normalize_to(
            &patterns,
            &["safe"],
            true,
            &mut state_budget
        ),
        PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal)
    );
    assert_eq!(state_budget.remaining_states, 1);
    assert_eq!(
        pattern_components_may_normalize_to(
            &patterns,
            &["safe"],
            true,
            &mut state_budget
        ),
        PatternReachability::Unknown
    );
    assert_eq!(state_budget.remaining_states, 0);

    let mut component_budget = PatternMatchBudget {
        remaining_states: MAX_PATTERN_MATCH_STATES,
        remaining_components: 4,
    };
    assert_eq!(
        pattern_components_may_normalize_to(
            &patterns,
            &["safe"],
            true,
            &mut component_budget
        ),
        PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal)
    );
    assert_eq!(component_budget.remaining_components, 0);
    assert_eq!(
        pattern_components_may_normalize_to(
            &patterns,
            &["safe"],
            true,
            &mut component_budget
        ),
        PatternReachability::Unknown
    );

    let safe_pattern = "/xa99z*/**/**/**/zz";
    assert!(
        evaluate_command(&format!("IFS=:; X='{safe_pattern}'; rm -rf $X")).is_none()
    );
    let fields = std::iter::repeat_n(safe_pattern, 500)
        .collect::<Vec<_>>()
        .join(":");
    let deny = evaluate_command(&format!("IFS=:; X='{fields}'; rm -rf $X")).unwrap();
    assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
}
```

- [ ] **Step 2: Run the focused unit test and verify RED**

Run:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::home_alias_classification_respects_field_splitting \
  -- --exact --nocapture --test-threads=1
```

Expected: FAIL because
`shopt -s globstar; .../**/../...; rm -rf $X` returns no deterministic
decision. The additional edge cases are coverage and may already deny through
existing lexical checks.

- [ ] **Step 3: Add RED provider regressions**

In
`home_alias_field_splitting_is_denied_before_model_inference_for_every_provider`
in `tests/hook_activity.rs`, add `"globstar parent traversal"` to the unsafe
case list. Build the command from each temporary HOME:

```rust
"globstar parent traversal" => (
    format!(
        "shopt -s globstar; IFS=:; X='{home_text}/**/../{home_name}'; rm -rf $X"
    ),
    Path::new("/"),
    Path::new("/"),
),
```

Add these cases to the model-backed control list:

```rust
"quoted globstar parent traversal",
"unrelated globstar parent traversal",
```

and construct them as:

```rust
"quoted globstar parent traversal" => {
    format!("X='{home_text}/**/../{home_name}'; rm -rf \"$X\"")
}
"unrelated globstar parent traversal" => {
    "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/../safe'; rm -rf $X"
        .to_string()
}
```

- [ ] **Step 4: Run the provider test and verify RED**

Run:

```bash
nix develop path:. --command cargo test --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: FAIL for `"globstar parent traversal"` because the hook allows it
after one model request.

- [ ] **Step 5: Introduce tri-state reachability and an aggregate budget**

Change the collection import and add the private types near the safety
constants:

```rust
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_PATTERN_MATCH_STATES: usize = 16_384;
const MAX_PATTERN_MATCH_COMPONENTS: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternMatchKind {
    DirectExpansion,
    ExpansionThenTraversal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternReachability {
    Reachable(PatternMatchKind),
    Unreachable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitTargetRisk {
    None,
    Root,
    HomeOrAncestor,
    UnsafeExpansion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternComponent {
    Literal(String),
    Globstar,
    Parent,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum ResolvedPatternComponent {
    Literal(usize),
    Any,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct PatternMatchState {
    pattern_index: usize,
    resolved: Vec<ResolvedPatternComponent>,
}

struct PatternMatchBudget {
    remaining_states: usize,
    remaining_components: usize,
}
```

At the start of `evaluate_in_process`, before iterating over parsed commands,
construct one shared budget:

```rust
let mut pattern_match_budget = PatternMatchBudget {
    remaining_states: MAX_PATTERN_MATCH_STATES,
    remaining_components: MAX_PATTERN_MATCH_COMPONENTS,
};
```

Pass `&mut pattern_match_budget` into
`split_target_risk`. Handle its outcomes at the call site:

```rust
match split_target_risk(
    target,
    &command_assignments,
    command_ifs_unknown,
    &mut pattern_match_budget,
) {
    SplitTargetRisk::Root => {
        return canonical_deny("irreversible-root-delete");
    }
    SplitTargetRisk::HomeOrAncestor => {
        return canonical_deny("irreversible-home-delete");
    }
    SplitTargetRisk::UnsafeExpansion => return expansion_target_deny(),
    SplitTargetRisk::None => {}
}
```

- [ ] **Step 6: Replace the boolean pattern helper with bounded matching**

Replace `lexical_pattern_parts` with a parser that preserves parent components:

```rust
fn pattern_parts(pattern: &str) -> Option<(bool, Vec<PatternComponent>)> {
    let mut absolute = false;
    let mut parts = Vec::new();
    for component in Path::new(pattern).components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => parts.push(PatternComponent::Parent),
            Component::Normal(part) => {
                let part = part.to_str()?;
                parts.push(if part == "**" {
                    PatternComponent::Globstar
                } else {
                    PatternComponent::Literal(part.to_string())
                });
            }
            Component::Prefix(_) => return None,
        }
    }
    Some((absolute, parts))
}
```

Keep the current lexical normalization and `pattern_components_may_match` logic
as private direct-expansion functions inside the new authoritative matcher.
Call that path first so patterns already conservatively accepted—including
components between multiple globstars—remain accepted, and report those matches
as `DirectExpansion`.

Bound state insertion before anything enters either the deduplication set or
queue. Literal states store pattern indices, not cloned strings. Charge twice
the resolved component count because a queued state is also retained in
`seen`:

```rust
fn enqueue_pattern_state(
    state: PatternMatchState,
    seen: &mut HashSet<PatternMatchState>,
    work: &mut VecDeque<PatternMatchState>,
    budget: &mut PatternMatchBudget,
) -> bool {
    if seen.contains(&state) {
        return true;
    }
    let stored_components = state.resolved.len().saturating_mul(2);
    if budget.remaining_states == 0
        || budget.remaining_components < stored_components
    {
        return false;
    }
    budget.remaining_states -= 1;
    budget.remaining_components -= stored_components;
    seen.insert(state.clone());
    work.push_back(state);
    true
}

fn pattern_components_may_normalize_to(
    patterns: &[PatternComponent],
    candidates: &[&str],
    absolute: bool,
    budget: &mut PatternMatchBudget,
) -> PatternReachability {
    let parent_count = patterns
        .iter()
        .filter(|part| matches!(part, PatternComponent::Parent))
        .count();
    let max_resolved = candidates
        .len()
        .saturating_add(parent_count)
        .saturating_add(patterns.len());
    let initial = PatternMatchState {
        pattern_index: 0,
        resolved: Vec::new(),
    };
    let mut work = VecDeque::new();
    let mut seen = HashSet::new();
    let mut unknown = false;
    if !enqueue_pattern_state(initial, &mut seen, &mut work, budget) {
        return PatternReachability::Unknown;
    }

    while let Some(state) = work.pop_front() {
        if state.pattern_index == patterns.len() {
            if state.resolved.len() == candidates.len()
                && state.resolved.iter().zip(candidates).all(|(part, candidate)| {
                    match part {
                        ResolvedPatternComponent::Literal(index) => match &patterns[*index] {
                            PatternComponent::Literal(pattern) => {
                                pattern_may_match_literal(pattern, candidate)
                            }
                            _ => unreachable!("literal state points to a literal pattern"),
                        },
                        ResolvedPatternComponent::Any => true,
                    }
                })
            {
                return PatternReachability::Reachable(
                    PatternMatchKind::ExpansionThenTraversal,
                );
            }
            continue;
        }

        match &patterns[state.pattern_index] {
            PatternComponent::Literal(_) => {
                let mut next = state;
                let literal_index = next.pattern_index;
                next.pattern_index += 1;
                next.resolved
                    .push(ResolvedPatternComponent::Literal(literal_index));
                if next.resolved.len() <= max_resolved
                    && !enqueue_pattern_state(next, &mut seen, &mut work, budget)
                {
                    return PatternReachability::Unknown;
                }
            }
            PatternComponent::Parent => {
                let mut next = state;
                next.pattern_index += 1;
                if next.resolved.pop().is_some() {
                    if !enqueue_pattern_state(next, &mut seen, &mut work, budget) {
                        return PatternReachability::Unknown;
                    }
                } else if absolute {
                    unknown = true;
                } else {
                    return PatternReachability::Unknown;
                }
            }
            PatternComponent::Globstar => {
                let available = max_resolved.saturating_sub(state.resolved.len());
                for consumed in 0..=available {
                    let mut next = state.clone();
                    next.pattern_index += 1;
                    next.resolved.extend(
                        std::iter::repeat_n(ResolvedPatternComponent::Any, consumed),
                    );
                    if !enqueue_pattern_state(next, &mut seen, &mut work, budget) {
                        return PatternReachability::Unknown;
                    }
                }
            }
        }
    }

    if unknown {
        PatternReachability::Unknown
    } else {
        PatternReachability::Unreachable
    }
}
```

Use the same `PatternMatchBudget` for every HOME prefix and relative suffix
candidate. Convert invalid HOME, candidate UTF-8 conversion, or pattern parsing
to `Unknown`. The authoritative helper first runs direct expansion and returns
`Reachable(DirectExpansion)` on a match; otherwise it runs the state matcher.
Merge results in priority order: `Reachable > Unknown > Unreachable`.

- [ ] **Step 7: Wire one authoritative matcher through split fields**

Replace `parameter_pattern_may_match_home`,
`split_target_may_reach_home_or_ancestor`,
`split_field_may_reach_home_or_ancestor`, and
`split_field_pattern_may_match_parts` with the authoritative matcher and a
`split_target_risk` combiner. Remove the HOME-pattern call from
`dynamic_target_is_dangerous` so each resolved field is analyzed only once.

The target-level combiner must:

```rust
if !target.can_split_fields {
    return SplitTargetRisk::None;
}
let Some(fields) = resolve_word_fields(target, assignments, ifs_unknown) else {
    return SplitTargetRisk::UnsafeExpansion;
};
for field in &fields {
    match pattern_reachability(field, &[], true, budget) {
        PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal) => {
            return SplitTargetRisk::Root;
        }
        PatternReachability::Reachable(PatternMatchKind::DirectExpansion)
        | PatternReachability::Unknown => return SplitTargetRisk::UnsafeExpansion,
        PatternReachability::Unreachable => {}
    }

    match pattern_reachability(field, &home, true, budget) {
        PatternReachability::Reachable(PatternMatchKind::DirectExpansion) => {
            return SplitTargetRisk::UnsafeExpansion;
        }
        PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal) => {
            return SplitTargetRisk::HomeOrAncestor;
        }
        PatternReachability::Unknown => return SplitTargetRisk::UnsafeExpansion,
        PatternReachability::Unreachable => {}
    }

    for ancestor_len in 1..home.len() {
        match pattern_reachability(field, &home[..ancestor_len], true, budget) {
            PatternReachability::Reachable(_) => {
                return SplitTargetRisk::HomeOrAncestor;
            }
            PatternReachability::Unknown => return SplitTargetRisk::UnsafeExpansion,
            PatternReachability::Unreachable => {}
        }
    }
}
SplitTargetRisk::None
```

The field-level combiner must also preserve the current exact absolute-prefix
and relative-suffix checks. Relative fields containing `ParentDir` return
`UnsafeExpansion`, because they do not have trusted cwd authority. Direct full
HOME matches preserve `unsafe-recursive-delete-expansion`; matches that depend
on post-expansion traversal and all HOME-ancestor matches use
`irreversible-home-delete`.

- [ ] **Step 8: Run focused tests and verify GREEN**

Run serially:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::home_alias_classification_respects_field_splitting \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test \
  brain::safety::tests::pathname_parent_match_budgets_are_shared_and_fail_closed \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: all exit `0`; unsafe provider cases deny with zero model requests,
and both new controls allow after exactly one model request.

- [ ] **Step 9: Run the prior safety corpus**

Run serially:

```bash
nix develop path:. --command cargo test safety -- --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  reopened_shell_safety_corpus_denies_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: all tests pass.

- [ ] **Step 10: Run full verification**

Run serially:

```bash
nix develop path:. --command cargo test --quiet -- --test-threads=1
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo build
git diff --check
```

Safely demonstrate real Bash expansion without invoking `rm`:

```bash
HOME=/home/alexander bash -O globstar -c \
  'IFS=:; X="/home/alexander/**/../alexander"; printf "<%s>\n" $X'
```

Expected output includes `</home/alexander/../alexander>`, which lexically
normalizes to trusted HOME.

Also inspect the complete diff and confirm every changed line belongs to xa99.
Run an independent review focused on fail-open conversions, insertion-time
budget enforcement, and control regressions. Update but do not close
`codexctl-xa99`. Do not commit or publish; report verification and request the
user's finish choice.

## Stress Test Results: XA99 Globstar Parent Traversal Plan

### Resolved Decisions

- Keep one atomic implementation task because matcher wiring and provider proof
  form one security boundary.
- Require the confirmed HOME globstar-parent case to be RED; treat other edge
  cases as coverage that may already deny.
- Replace the boolean helper with one tri-state matcher while preserving direct
  expansion and expansion-then-traversal provenance.
- Charge state and stored-component budgets before inserting unique work.
- Preserve exact existing rule IDs, provider controls, and the conservative
  multi-globstar envelope.
- Run focused, corpus, full repository, safe Bash probe, and independent review
  gates before handoff.

### Changes Made

- Renamed historical `LegacyPattern` terminology to semantic
  `DirectExpansion` and `ExpansionThenTraversal` kinds.
- Replaced per-dequeue budgeting with insertion-time aggregate limits.
- Added indexed literal states and a dual-copy stored-component budget.
- Clarified RED expectations, authoritative helper replacement, safe Bash
  evidence, and uncommitted handoff.

### Deferred / Parking Lot

- None. Commit, push, PR, merge, and publication remain outside the authorized
  implementation scope.

### Confidence Assessment

- Overall: High
- Areas of concern: implementation review must confirm every `Unknown` reaches
  the unsafe-expansion denial and no successor enters the queue before both
  budgets are charged.
