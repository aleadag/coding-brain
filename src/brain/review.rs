//! Interactive review of brain decisions.
//!
//! `cbrain --brain-review` surfaces the highest-value decisions to triage:
//! brain-was-right counterfactuals, Critical-tier safety hits, and
//! high-confidence calibration misses. The user marks each as canonical
//! (teaching material) or skips. Canonical marks are stored in
//! the Coding Brain canonical decision log and get a large score boost in
//! few-shot retrieval — turning each review pass into supervised training.
//!
//! Implementation is plain stdin/stdout. A full ratatui screen integrated
//! with the dashboard is tracked as a follow-up — see issue noted in the
//! PR opening this module.

use std::io::{self, BufRead, Write};

use super::decisions::{DecisionRecord, mark_canonical, read_all_decisions};
use super::metrics::{compute_counterfactuals, compute_tier_stats};
use super::risk::{RiskTier, classify_risk};

/// A scored review candidate.
#[derive(Debug, Clone)]
pub struct ReviewItem {
    pub record: DecisionRecord,
    pub reason: String,
    pub score: i32,
}

pub(crate) fn review_source_identity(record: &DecisionRecord) -> Vec<u8> {
    coding_brain_core::runtime::DecisionSummary::from(record).review_source_identity()
}

pub(crate) fn review_display_id(record: &DecisionRecord, _source_identity: &[u8]) -> String {
    coding_brain_core::runtime::DecisionSummary::from(record).review_display_id()
}

/// Build the prioritized review queue.
pub fn build_queue(decisions: &[DecisionRecord]) -> Vec<ReviewItem> {
    let mut items: Vec<ReviewItem> = Vec::new();
    let reviewable = decisions
        .iter()
        .filter(|decision| decision.canonical != Some(true))
        .collect::<Vec<_>>();
    // compute_counterfactuals (and the other compute_* helpers) operate on
    // the core `DecisionSummary` DTO since the metrics surface is shared
    // with the TUI. Project once at the call site.
    let summaries: Vec<coding_brain_core::runtime::DecisionSummary> = reviewable
        .iter()
        .map(|decision| (*decision).into())
        .collect();
    let cfs = compute_counterfactuals(&summaries);

    for cf in &cfs {
        if cf.brain_was_right {
            if let Some(record) = find_by_id(&reviewable, cf.decision_id.as_deref()) {
                items.push(ReviewItem {
                    record: record.clone(),
                    reason: format!("Brain was right (counterfactual): {}", cf.outcome_summary),
                    score: 100,
                });
            }
        }
    }

    for d in reviewable {
        if d.brain_action.is_empty() {
            continue;
        }
        let tier = classify_risk(d.tool.as_deref(), d.command.as_deref());
        // Critical-tier disagreements get a high priority regardless of outcome.
        if matches!(tier, RiskTier::Critical) && d.is_negative() && d.brain_action == "approve" {
            items.push(ReviewItem {
                record: d.clone(),
                reason: "Critical-tier false-approve (safety review)".into(),
                score: 90,
            });
        }
        // High-confidence misses: brain >= 80% confident but user disagreed.
        if d.is_negative() && d.brain_confidence >= 0.80 {
            items.push(ReviewItem {
                record: d.clone(),
                reason: format!(
                    "High-confidence miss ({:.0}% confidence)",
                    d.brain_confidence * 100.0
                ),
                score: 60 + ((d.brain_confidence - 0.80) * 100.0) as i32,
            });
        }
    }

    // De-duplicate counterfactual and risk reasons for the same source decision.
    items.sort_by(|a, b| {
        review_source_identity(&a.record)
            .cmp(&review_source_identity(&b.record))
            .then_with(|| b.score.cmp(&a.score))
    });
    items.dedup_by(|a, b| review_source_identity(&a.record) == review_source_identity(&b.record));
    items.sort_by_key(|x| std::cmp::Reverse(x.score));
    items
}

fn find_by_id<'a>(
    decisions: &[&'a DecisionRecord],
    id: Option<&str>,
) -> Option<&'a DecisionRecord> {
    let id = id?;
    decisions
        .iter()
        .copied()
        .find(|decision| decision.decision_id.as_deref() == Some(id))
}

/// Run an interactive review pass. Returns the number of items marked canonical.
pub fn run_interactive() -> usize {
    let decisions = read_all_decisions();
    let queue = build_queue(&decisions);

    println!("Brain Review");
    println!("============");
    println!();

    if queue.is_empty() {
        println!("No review-worthy decisions in the queue. Either:");
        println!("  - The brain has been right on every confident call (great).");
        println!("  - Outcome attribution hasn't kicked in yet (try after more usage).");
        println!();
        println!("Run `cbrain --brain-stats scorecard` to see overall health.");
        return 0;
    }

    println!(
        "{} review candidates in queue, ordered by review value.",
        queue.len()
    );
    println!();
    println!("For each: [m]ark canonical · [n]ote + mark · [s]kip · [d]etails · [q]uit");
    println!();

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut marked = 0usize;
    let total = queue.len();
    for (i, item) in queue.iter().enumerate() {
        println!("[{}/{}]  reason: {}", i + 1, total, item.reason);
        print_summary_line(&item.record);
        println!();

        loop {
            print!("  > ");
            let _ = io::stdout().flush();
            let mut buf = String::new();
            if reader.read_line(&mut buf).is_err() {
                println!();
                println!("Stopping review.");
                return marked;
            }
            let cmd = buf.trim();
            match cmd {
                "m" | "mark" => {
                    if let Some(id) = item
                        .record
                        .decision_id
                        .as_deref()
                        .filter(|id| !id.trim().is_empty())
                    {
                        match mark_canonical(id, None) {
                            Ok(()) => {
                                println!("  ✓ marked canonical");
                                marked += 1;
                            }
                            Err(e) => {
                                println!("  ! could not write: {e}");
                            }
                        }
                    } else {
                        println!("  ! no decision_id — older record, can't mark");
                    }
                    break;
                }
                "n" | "note" => {
                    print!("    note: ");
                    let _ = io::stdout().flush();
                    let mut note = String::new();
                    let _ = reader.read_line(&mut note);
                    if let Some(id) = item
                        .record
                        .decision_id
                        .as_deref()
                        .filter(|id| !id.trim().is_empty())
                    {
                        match mark_canonical(id, Some(note.trim())) {
                            Ok(()) => {
                                println!("  ✓ marked canonical with note");
                                marked += 1;
                            }
                            Err(e) => {
                                println!("  ! could not write: {e}");
                            }
                        }
                    } else {
                        println!("  ! no decision_id — older record, can't mark");
                    }
                    break;
                }
                "d" | "details" => {
                    print_full_details(&item.record);
                    // Loop again for an action on the same item.
                }
                "s" | "skip" | "" => {
                    break;
                }
                "q" | "quit" | "exit" => {
                    println!();
                    println!("Reviewed {} item(s), marked {marked}.", i + 1);
                    return marked;
                }
                _ => {
                    println!("  unknown: '{}' — try m / n / s / d / q", cmd);
                }
            }
        }
        println!();
    }

    println!("Done. Marked {marked} of {total} canonical.");
    marked
}

fn print_summary_line(d: &DecisionRecord) {
    let tier = classify_risk(d.tool.as_deref(), d.command.as_deref());
    println!(
        "  tier={}  tool={}  brain={} (conf {:.0}%)  user={}",
        tier,
        d.tool.as_deref().unwrap_or("?"),
        d.brain_action,
        d.brain_confidence * 100.0,
        d.user_action,
    );
    if let Some(cmd) = &d.command {
        let short = if cmd.len() > 100 {
            format!("{}…", &cmd[..100])
        } else {
            cmd.clone()
        };
        println!("  cmd: {}", short);
    }
}

fn print_full_details(d: &DecisionRecord) {
    println!("  --- details ---");
    println!(
        "  decision_id:      {}",
        d.decision_id.as_deref().unwrap_or("(none)")
    );
    println!("  project:          {}", d.project);
    println!(
        "  tool:             {}",
        d.tool.as_deref().unwrap_or("(none)")
    );
    if let Some(cmd) = &d.command {
        println!("  command:          {cmd}");
    }
    println!("  brain_action:     {}", d.brain_action);
    println!("  brain_confidence: {:.2}", d.brain_confidence);
    println!("  brain_reasoning:  {}", d.brain_reasoning);
    println!("  user_action:      {}", d.user_action);
    if let Some(reason) = &d.override_reason {
        println!("  override_reason:  {reason}");
    }
    if let Some(ms) = d.brain_decision_ms {
        println!("  brain_latency:    {ms} ms");
    }
    if let Some(hit) = d.cache_hit {
        println!("  cache_hit:        {hit}");
    }
    if let Some(ctx) = &d.context {
        if let Some(context_pct) = ctx.context_pct {
            println!("  context_pct:      {context_pct}%");
        }
        println!("  model:            {}", ctx.model);
    }
    println!();
}

/// One-shot non-interactive helper for `--mark <id>` (called from the
/// counterfactual report).
pub fn mark_by_id(decision_id: &str, note: Option<&str>) -> Result<(), String> {
    if decision_id.trim().is_empty() {
        return Err("decision id must not be empty".into());
    }
    mark_canonical(decision_id, note)
}

/// Print the review queue (non-interactive) — useful for piping into other tools.
pub fn print_queue() {
    let decisions = read_all_decisions();
    let queue = build_queue(&decisions);
    let summaries: Vec<coding_brain_core::runtime::DecisionSummary> =
        decisions.iter().map(Into::into).collect();
    let tier_stats = compute_tier_stats(&summaries);

    println!("Review Queue ({} item(s))", queue.len());
    println!(
        "======================{}",
        "=".repeat(queue.len().to_string().len())
    );
    println!();
    println!("Per-tier sample sizes:");
    for s in &tier_stats {
        println!("  {:<10}  n = {}", s.tier.label(), s.n);
    }
    println!();
    for (i, item) in queue.iter().enumerate() {
        println!("{}. [{}]  {}", i + 1, item.score, item.reason);
        print_summary_line(&item.record);
        println!();
    }
    if queue.is_empty() {
        println!("(empty)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_brain_core::provider::AgentProvider;

    fn decision(id: &str, brain_action: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            provider: AgentProvider::Codex,
            timestamp: "1".into(),
            pid: 7,
            project: "project".into(),
            tool: Some("Bash".into()),
            command: Some("cargo test".into()),
            brain_action: brain_action.into(),
            brain_confidence: 0.9,
            brain_reasoning: "reason".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: super::super::decisions::DecisionType::Session,
            suggested_at: Some(1),
            resolved_at: Some(2),
            override_reason: None,
            decision_id: Some(id.into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    #[test]
    fn canonical_counterfactual_cannot_reenter_review_queue() {
        let mut canonical = decision("canonical", "deny", "reject");
        canonical.canonical = Some(true);
        let mut outcome = decision("outcome", "", "auto");
        outcome.outcome = Some(super::super::decisions::DecisionOutcome::Error(
            "failed".into(),
        ));

        assert!(build_queue(&[canonical, outcome]).is_empty());
    }

    #[test]
    fn mark_by_id_rejects_empty_and_whitespace_ids() {
        assert!(mark_by_id("", None).is_err());
        assert!(mark_by_id("   ", None).is_err());
    }
}
