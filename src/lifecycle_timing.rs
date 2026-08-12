#![allow(dead_code)]

use std::io::Write;
use std::time::{Duration, Instant};

use coding_brain_core::provider::AgentProvider;

pub(crate) const HOOK_BUDGET: Duration = Duration::from_millis(1500);

pub(crate) trait MonotonicClock: Clone {
    fn now(&self) -> Instant;
}

#[derive(Clone, Copy)]
pub(crate) struct SystemClock;

impl MonotonicClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub(crate) struct HookBudget<C = SystemClock> {
    clock: C,
    started: Instant,
    deadline: Instant,
}

impl HookBudget<SystemClock> {
    pub(crate) fn from_start(started: Instant) -> Self {
        Self {
            clock: SystemClock,
            started,
            deadline: started + HOOK_BUDGET,
        }
    }
}

impl<C: MonotonicClock> HookBudget<C> {
    pub(crate) fn with_clock(clock: C, budget: Duration) -> Self {
        let started = clock.now();
        Self {
            started,
            deadline: started + budget,
            clock,
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(self.clock.now())
    }

    pub(crate) fn allowance(&self, cap: Duration) -> Duration {
        self.remaining().min(cap)
    }

    pub(crate) fn child_deadline(&self, cap: Duration) -> Option<Instant> {
        let now = self.clock.now();
        let allowance = self.deadline.saturating_duration_since(now).min(cap);
        (!allowance.is_zero()).then_some(now + allowance)
    }

    pub(crate) fn optional_child_deadline(
        &self,
        cap: Duration,
        reserve: Duration,
    ) -> Option<Instant> {
        let now = self.clock.now();
        let allowance = self
            .deadline
            .saturating_duration_since(now)
            .saturating_sub(reserve)
            .min(cap);
        (!allowance.is_zero()).then_some(now + allowance)
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn started(&self) -> Instant {
        self.started
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookEventClass {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    SubagentStart,
    SubagentStop,
    Other,
}

impl HookEventClass {
    pub(crate) fn from_lifecycle_name(name: &str) -> Self {
        match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "Stop" => Self::Stop,
            "SubagentStart" => Self::SubagentStart,
            "SubagentStop" => Self::SubagentStop,
            _ => Self::Other,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookStage {
    CliInput,
    Parse,
    ParentDiscovery,
    ProjectCache,
    ProjectGit,
    SqliteOpen,
    LifecycleCommit,
    PostToolCorrelation,
    ActivityCommit,
    CacheRefresh,
    Total,
}

impl HookStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::CliInput => "cli_input",
            Self::Parse => "parse",
            Self::ParentDiscovery => "parent_discovery",
            Self::ProjectCache => "project_cache",
            Self::ProjectGit => "project_git",
            Self::SqliteOpen => "sqlite_open",
            Self::LifecycleCommit => "lifecycle_commit",
            Self::PostToolCorrelation => "posttool_correlation",
            Self::ActivityCommit => "activity_commit",
            Self::CacheRefresh => "cache_refresh",
            Self::Total => "total",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookOutcome {
    Success,
    Timeout,
    InputTooLarge,
    InputRead,
    StorageUnavailable,
    Rejected,
    Error,
    CacheHit,
    CacheMiss,
    CacheInvalid,
    CacheBypassed,
    NonCacheable,
    DiscoveryFailure,
}

impl HookOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Timeout => "timeout",
            Self::InputTooLarge => "input_too_large",
            Self::InputRead => "input_read",
            Self::StorageUnavailable => "storage_unavailable",
            Self::Rejected => "rejected",
            Self::Error => "error",
            Self::CacheHit => "cache_hit",
            Self::CacheMiss => "cache_miss",
            Self::CacheInvalid => "cache_invalid",
            Self::CacheBypassed => "cache_bypassed",
            Self::NonCacheable => "non_cacheable",
            Self::DiscoveryFailure => "discovery_failure",
        }
    }
}

pub(crate) struct TimingRecord {
    provider: AgentProvider,
    event: HookEventClass,
    stage: HookStage,
    outcome: HookOutcome,
    elapsed_ms: u128,
    remaining_ms: u128,
}

impl TimingRecord {
    pub(crate) fn new(
        provider: AgentProvider,
        event: HookEventClass,
        stage: HookStage,
        outcome: HookOutcome,
        elapsed_ms: u128,
        remaining_ms: u128,
    ) -> Self {
        Self {
            provider,
            event,
            stage,
            outcome,
            elapsed_ms,
            remaining_ms,
        }
    }
}

pub(crate) fn format_timing(record: TimingRecord) -> String {
    format!(
        "cbrain_hook_timing v=1 provider={} event={} stage={} outcome={} elapsed_ms={} remaining_ms={}\n",
        provider_name(record.provider),
        record.event.as_str(),
        record.stage.as_str(),
        record.outcome.as_str(),
        record.elapsed_ms,
        record.remaining_ms,
    )
}

fn provider_name(provider: AgentProvider) -> &'static str {
    match provider {
        AgentProvider::Codex => "codex",
        AgentProvider::Claude => "claude",
        AgentProvider::Antigravity => "antigravity",
    }
}

pub(crate) struct HookTiming {
    provider: AgentProvider,
    event: HookEventClass,
    started: Instant,
    deadline: Instant,
}

impl HookTiming {
    pub(crate) fn new(provider: AgentProvider, event: HookEventClass, budget: &HookBudget) -> Self {
        Self {
            provider,
            event,
            started: budget.started(),
            deadline: budget.deadline(),
        }
    }

    pub(crate) fn set_event(&mut self, event: HookEventClass) {
        self.event = event;
    }

    pub(crate) fn finish(&self, stage: HookStage, outcome: HookOutcome) {
        if std::env::var_os("CBRAIN_HOOK_TIMING").as_deref() != Some("1".as_ref()) {
            return;
        }
        let now = Instant::now();
        let record = TimingRecord::new(
            self.provider,
            self.event,
            stage,
            outcome,
            now.duration_since(self.started).as_millis(),
            self.deadline.saturating_duration_since(now).as_millis(),
        );
        let _ = std::io::stderr().write_all(format_timing(record).as_bytes());
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FakeClock(std::sync::Arc<std::sync::Mutex<Instant>>);

#[cfg(test)]
impl FakeClock {
    pub(crate) fn at(now: Instant) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(now)))
    }

    pub(crate) fn advance(&self, duration: Duration) {
        let mut now = self.0.lock().expect("fake clock lock");
        *now += duration;
    }

    pub(crate) fn now(&self) -> Instant {
        *self.0.lock().expect("fake clock lock")
    }
}

#[cfg(test)]
impl MonotonicClock for FakeClock {
    fn now(&self) -> Instant {
        self.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_share_the_entry_budget_and_timing_is_closed() {
        let clock = FakeClock::at(Instant::now());
        let budget = HookBudget::with_clock(clock.clone(), Duration::from_millis(1500));
        clock.advance(Duration::from_millis(1200));
        assert_eq!(budget.remaining(), Duration::from_millis(300));
        assert_eq!(
            budget.allowance(Duration::from_millis(500)),
            Duration::from_millis(300)
        );

        let line = format_timing(TimingRecord::new(
            AgentProvider::Codex,
            HookEventClass::UserPromptSubmit,
            HookStage::ProjectGit,
            HookOutcome::Timeout,
            250,
            1010,
        ));
        assert_eq!(
            line,
            "cbrain_hook_timing v=1 provider=codex event=user_prompt_submit stage=project_git outcome=timeout elapsed_ms=250 remaining_ms=1010\n"
        );
        assert!(!line.contains('/'));
    }

    #[test]
    fn optional_child_deadline_preserves_storage_reserve() {
        let clock = FakeClock::at(Instant::now());
        let budget = HookBudget::with_clock(clock.clone(), Duration::from_millis(1500));
        clock.advance(Duration::from_millis(1200));

        assert_eq!(
            budget.optional_child_deadline(Duration::from_millis(250), Duration::from_millis(500)),
            None
        );
    }

    #[test]
    fn lifecycle_event_names_stay_closed() {
        assert_eq!(
            HookEventClass::from_lifecycle_name("SessionStart"),
            HookEventClass::SessionStart
        );
        assert_eq!(
            HookEventClass::from_lifecycle_name("PreToolUse"),
            HookEventClass::PreToolUse
        );
        assert_eq!(
            HookEventClass::from_lifecycle_name("untrusted-event"),
            HookEventClass::Other
        );
    }

    #[test]
    fn lifecycle_pipeline_stage_names_are_closed_and_ordered() {
        let stages = [
            HookStage::CliInput,
            HookStage::Parse,
            HookStage::ParentDiscovery,
            HookStage::ProjectCache,
            HookStage::ProjectGit,
            HookStage::SqliteOpen,
            HookStage::LifecycleCommit,
            HookStage::PostToolCorrelation,
            HookStage::ActivityCommit,
            HookStage::CacheRefresh,
            HookStage::Total,
        ];

        assert_eq!(
            stages.map(HookStage::as_str),
            [
                "cli_input",
                "parse",
                "parent_discovery",
                "project_cache",
                "project_git",
                "sqlite_open",
                "lifecycle_commit",
                "posttool_correlation",
                "activity_commit",
                "cache_refresh",
                "total",
            ]
        );
    }

    #[test]
    fn project_cache_outcomes_are_closed_without_treating_misses_as_errors() {
        let outcomes = [
            HookOutcome::CacheHit,
            HookOutcome::CacheMiss,
            HookOutcome::CacheInvalid,
            HookOutcome::CacheBypassed,
            HookOutcome::NonCacheable,
            HookOutcome::DiscoveryFailure,
        ];

        assert_eq!(
            outcomes.map(HookOutcome::as_str),
            [
                "cache_hit",
                "cache_miss",
                "cache_invalid",
                "cache_bypassed",
                "non_cacheable",
                "discovery_failure",
            ]
        );
    }
}
