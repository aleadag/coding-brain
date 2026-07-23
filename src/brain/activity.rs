#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityDiagnostics, ActivityEvent, ActivityItem, ActivityKind,
    ActivityOutcome, ActivitySnapshot, ActivityState, AttentionItem, DEFAULT_INTERRUPTED_AFTER_MS,
    DeliveryState, MAX_ACTIVITY_EVENT_BYTES, MIN_ACTIVITY_SCHEMA_VERSION, SnapshotLimits,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

const LOCK_RETRY: Duration = Duration::from_millis(5);
const MAX_DIAGNOSTIC_OFFSETS: usize = 100;
const MAX_RETAINED_INTERRUPTED_LIFECYCLES: usize = 256;

#[derive(Debug, Clone)]
pub struct ActivityLimits {
    pub lock_timeout_ms: u64,
    pub compact_at_bytes: u64,
    pub retained_lifecycles: usize,
}

impl Default for ActivityLimits {
    fn default() -> Self {
        Self {
            lock_timeout_ms: 100,
            compact_at_bytes: 32 * 1024 * 1024,
            retained_lifecycles: 10_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivityStore {
    path: PathBuf,
    lock_path: PathBuf,
    limits: ActivityLimits,
    now_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ActivityLifecycle {
    terminal_state: ActivityState,
}

impl ActivityLifecycle {
    pub fn terminal_state(&self) -> ActivityState {
        self.terminal_state
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActivityLog {
    events: Vec<ActivityEvent>,
    diagnostics: ActivityDiagnostics,
}

impl ActivityLog {
    pub fn events(&self) -> &[ActivityEvent] {
        &self.events
    }

    pub fn diagnostics(&self) -> &ActivityDiagnostics {
        &self.diagnostics
    }

    pub fn activity(&self, activity_id: &str) -> Option<ActivityLifecycle> {
        let mut latest = None;
        let mut terminal = None;
        for event in self
            .events
            .iter()
            .filter(|event| event.activity_id == activity_id)
        {
            latest = Some(event.state);
            if terminal.is_none() && event.state.is_terminal() {
                terminal = Some(event.state);
            }
        }
        terminal
            .or(latest)
            .map(|terminal_state| ActivityLifecycle { terminal_state })
    }

    pub fn complete_lifecycles(&self) -> usize {
        self.events
            .iter()
            .filter(|event| event.state.is_terminal())
            .map(|event| event.activity_id.as_str())
            .collect::<HashSet<_>>()
            .len()
    }
}

#[derive(Debug)]
pub enum ActivityStoreError {
    Io(io::Error),
    LockTimeout,
    Serialization(serde_json::Error),
    UnsupportedSchema(u32),
    InvalidEvent,
    EventTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicReservationOutcome {
    Reserved,
    Duplicate,
    Cooldown,
}

impl fmt::Display for ActivityStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "activity store I/O failed: {error}"),
            Self::LockTimeout => formatter.write_str("activity store lock timed out"),
            Self::Serialization(error) => {
                write!(formatter, "activity serialization failed: {error}")
            }
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported activity schema {version}")
            }
            Self::InvalidEvent => formatter.write_str("activity event payload is inconsistent"),
            Self::EventTooLarge => formatter.write_str("activity event exceeds its size limit"),
        }
    }
}

impl std::error::Error for ActivityStoreError {}

impl From<io::Error> for ActivityStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ActivityStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticRow {
    schema_version: u32,
    diagnostic: StoreDiagnostic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoreDiagnostic {
    TruncatedTail { discarded_bytes: u64 },
    MalformedRows { count: usize },
}

struct LockGuard<'a> {
    file: &'a File,
}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.file);
    }
}

impl ActivityStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            limits: ActivityLimits::default(),
            now_ms: None,
        }
    }

    pub fn with_limits(mut self, limits: ActivityLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_clock(mut self, now_ms: u64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }

    pub fn append(&self, event: ActivityEvent) -> Result<(), ActivityStoreError> {
        if event.schema_version != ACTIVITY_SCHEMA_VERSION {
            return Err(ActivityStoreError::UnsupportedSchema(event.schema_version));
        }
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Exclusive)?;
        self.append_events_unlocked(&[event])
    }

    pub(crate) fn append_from_snapshot<F>(&self, build: F) -> Result<(), ActivityStoreError>
    where
        F: FnOnce(&ActivityLog) -> Vec<ActivityEvent>,
    {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Exclusive)?;
        let log = self.read_unlocked()?;
        let events = build(&log);
        self.append_events_unlocked(&events)
    }

    fn append_events_unlocked(&self, events: &[ActivityEvent]) -> Result<(), ActivityStoreError> {
        let serialized = events
            .iter()
            .map(|event| {
                if event.schema_version != ACTIVITY_SCHEMA_VERSION {
                    return Err(ActivityStoreError::UnsupportedSchema(event.schema_version));
                }
                let event = event.clone().normalized();
                if !event.has_consistent_payload() {
                    return Err(ActivityStoreError::InvalidEvent);
                }
                let mut serialized = serde_json::to_vec(&event)?;
                if serialized.len() > MAX_ACTIVITY_EVENT_BYTES {
                    return Err(ActivityStoreError::EventTooLarge);
                }
                serialized.push(b'\n');
                Ok(serialized)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        set_file_mode(&file)?;
        if let Some(discarded_bytes) = repair_tail(&mut file)? {
            let row = DiagnosticRow {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                diagnostic: StoreDiagnostic::TruncatedTail { discarded_bytes },
            };
            let mut diagnostic = serde_json::to_vec(&row)?;
            diagnostic.push(b'\n');
            file.seek(SeekFrom::End(0))?;
            file.write_all(&diagnostic)?;
        }
        file.seek(SeekFrom::End(0))?;
        for row in serialized {
            file.write_all(&row)?;
        }
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    pub(crate) fn reserve_recovery_event(
        &self,
        event: ActivityEvent,
        cooldown_ms: u64,
    ) -> Result<AtomicReservationOutcome, ActivityStoreError> {
        if event.schema_version != ACTIVITY_SCHEMA_VERSION {
            return Err(ActivityStoreError::UnsupportedSchema(event.schema_version));
        }
        let event = event.normalized();
        if event.rule_id.as_deref() != Some("recovery_reservation")
            || !event.has_consistent_payload()
        {
            return Err(ActivityStoreError::InvalidEvent);
        }
        let mut serialized = serde_json::to_vec(&event)?;
        if serialized.len() > MAX_ACTIVITY_EVENT_BYTES {
            return Err(ActivityStoreError::EventTooLarge);
        }
        serialized.push(b'\n');

        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Exclusive)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        set_file_mode(&file)?;
        if let Some(discarded_bytes) = repair_tail(&mut file)? {
            let row = DiagnosticRow {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                diagnostic: StoreDiagnostic::TruncatedTail { discarded_bytes },
            };
            let mut diagnostic = serde_json::to_vec(&row)?;
            diagnostic.push(b'\n');
            file.seek(SeekFrom::End(0))?;
            file.write_all(&diagnostic)?;
        }

        let log = self.read_unlocked()?;
        let same_session = |candidate: &ActivityEvent| {
            candidate.rule_id.as_deref() == Some("recovery_reservation")
                && candidate
                    .session
                    .as_ref()
                    .zip(event.session.as_ref())
                    .is_some_and(|(candidate, current)| {
                        candidate.provider == current.provider
                            && candidate.session_id == current.session_id
                    })
        };
        if log
            .events()
            .iter()
            .any(|candidate| same_session(candidate) && candidate.activity_id == event.activity_id)
        {
            return Ok(AtomicReservationOutcome::Duplicate);
        }
        if log.events().iter().rev().any(|candidate| {
            same_session(candidate)
                && event
                    .recorded_at_ms
                    .saturating_sub(candidate.recorded_at_ms)
                    < cooldown_ms
        }) {
            return Ok(AtomicReservationOutcome::Cooldown);
        }

        file.seek(SeekFrom::End(0))?;
        file.write_all(&serialized)?;
        file.flush()?;
        file.sync_data()?;
        Ok(AtomicReservationOutcome::Reserved)
    }

    pub(crate) fn append_if_absent(
        &self,
        event: ActivityEvent,
    ) -> Result<bool, ActivityStoreError> {
        if event.schema_version != ACTIVITY_SCHEMA_VERSION {
            return Err(ActivityStoreError::UnsupportedSchema(event.schema_version));
        }
        let event = event.normalized();
        if !event.has_consistent_payload() {
            return Err(ActivityStoreError::InvalidEvent);
        }
        let mut serialized = serde_json::to_vec(&event)?;
        if serialized.len() > MAX_ACTIVITY_EVENT_BYTES {
            return Err(ActivityStoreError::EventTooLarge);
        }
        serialized.push(b'\n');
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Exclusive)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        set_file_mode(&file)?;
        if let Some(discarded_bytes) = repair_tail(&mut file)? {
            let row = DiagnosticRow {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                diagnostic: StoreDiagnostic::TruncatedTail { discarded_bytes },
            };
            let mut diagnostic = serde_json::to_vec(&row)?;
            diagnostic.push(b'\n');
            file.seek(SeekFrom::End(0))?;
            file.write_all(&diagnostic)?;
        }
        if self
            .read_unlocked()?
            .events()
            .iter()
            .any(|candidate| candidate.activity_id == event.activity_id)
        {
            return Ok(false);
        }
        file.seek(SeekFrom::End(0))?;
        file.write_all(&serialized)?;
        file.flush()?;
        file.sync_data()?;
        Ok(true)
    }

    pub fn read(&self) -> Result<ActivityLog, ActivityStoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Shared)?;
        self.read_unlocked()
    }

    pub fn snapshot(&self, limits: SnapshotLimits) -> Result<ActivitySnapshot, ActivityStoreError> {
        let log = self.read()?;
        Ok(project_snapshot(
            log,
            limits,
            self.now_ms.unwrap_or_else(epoch_ms),
        ))
    }

    pub fn compact_if_needed(&self) -> Result<bool, ActivityStoreError> {
        let size = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if size < self.limits.compact_at_bytes {
            return Ok(false);
        }

        let lock = self.open_lock()?;
        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let _guard = LockGuard { file: &lock };
        if fs::metadata(&self.path)?.len() < self.limits.compact_at_bytes {
            return Ok(false);
        }

        let log = self.read_unlocked()?;
        let mut lifecycle_recency = HashMap::<&str, (bool, u64)>::new();
        for event in &log.events {
            let entry = lifecycle_recency
                .entry(&event.activity_id)
                .or_insert((false, event.recorded_at_ms));
            if event.state.is_terminal() {
                if entry.0 {
                    continue;
                }
                entry.0 = true;
            }
            entry.1 = entry.1.max(event.recorded_at_ms);
        }
        let mut completed = lifecycle_recency
            .into_iter()
            .filter_map(|(activity_id, (complete, recency))| {
                complete.then_some((activity_id, recency))
            })
            .collect::<Vec<_>>();
        completed.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        let retained = completed
            .into_iter()
            .take(self.limits.retained_lifecycles)
            .map(|(activity_id, _)| activity_id)
            .collect::<HashSet<_>>();
        let complete_ids = log
            .events
            .iter()
            .filter(|event| event.state.is_terminal())
            .map(|event| event.activity_id.as_str())
            .collect::<HashSet<_>>();
        let now_ms = self.now_ms.unwrap_or_else(epoch_ms);
        let incomplete = log
            .events
            .iter()
            .filter(|event| !complete_ids.contains(event.activity_id.as_str()))
            .fold(HashMap::<&str, u64>::new(), |mut recency, event| {
                recency
                    .entry(&event.activity_id)
                    .and_modify(|time| *time = (*time).max(event.recorded_at_ms))
                    .or_insert(event.recorded_at_ms);
                recency
            })
            .into_iter()
            .collect::<Vec<_>>();
        let mut interrupted = incomplete
            .iter()
            .copied()
            .filter(|(_, recency)| now_ms.saturating_sub(*recency) > DEFAULT_INTERRUPTED_AFTER_MS)
            .collect::<Vec<_>>();
        interrupted.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        let mut retained_incomplete = incomplete
            .iter()
            .copied()
            .filter(|(_, recency)| now_ms.saturating_sub(*recency) <= DEFAULT_INTERRUPTED_AFTER_MS)
            .map(|(activity_id, _)| activity_id)
            .collect::<HashSet<_>>();
        retained_incomplete.extend(
            interrupted
                .into_iter()
                .take(MAX_RETAINED_INTERRUPTED_LIFECYCLES)
                .map(|(activity_id, _)| activity_id),
        );

        let parent = parent_dir(&self.path);
        let mut temporary = tempfile::Builder::new()
            .prefix("activity.tmp-")
            .tempfile_in(parent)?;
        set_file_mode(temporary.as_file())?;
        if log.diagnostics.truncated_tails > 0 {
            write_diagnostic(
                &mut temporary,
                StoreDiagnostic::TruncatedTail {
                    discarded_bytes: log.diagnostics.discarded_tail_bytes,
                },
            )?;
        }
        if log.diagnostics.malformed_rows > 0 {
            write_diagnostic(
                &mut temporary,
                StoreDiagnostic::MalformedRows {
                    count: log.diagnostics.malformed_rows,
                },
            )?;
        }
        for event in &log.events {
            let activity_id = event.activity_id.as_str();
            if retained.contains(activity_id) || retained_incomplete.contains(activity_id) {
                serde_json::to_writer(&mut temporary, event)?;
                temporary.write_all(b"\n")?;
            }
        }
        temporary.flush()?;
        temporary.as_file().sync_data()?;
        temporary
            .persist(&self.path)
            .map_err(|error| ActivityStoreError::Io(error.error))?;
        Ok(true)
    }

    fn open_lock(&self) -> Result<File, ActivityStoreError> {
        let parent = parent_dir(&self.path);
        fs::create_dir_all(parent)?;
        set_dir_mode(parent)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)?;
        set_file_mode(&lock)?;
        Ok(lock)
    }

    fn read_unlocked(&self) -> Result<ActivityLog, ActivityStoreError> {
        let mut contents = Vec::new();
        match File::open(&self.path) {
            Ok(mut file) => {
                file.read_to_end(&mut contents)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ActivityLog::default());
            }
            Err(error) => return Err(error.into()),
        }

        let mut log = ActivityLog::default();
        let mut activity_kinds = HashMap::<String, ActivityKind>::new();
        let mut offset = 0_u64;
        for raw_line in contents.split_inclusive(|byte| *byte == b'\n') {
            let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
            if !line.is_empty() {
                if let Ok(event) = serde_json::from_slice::<ActivityEvent>(line) {
                    let kind_was_absent = serde_json::from_slice::<serde_json::Value>(line)?
                        .get("kind")
                        .is_none();
                    let mut event = event;
                    if kind_was_absent && event.activity_id.starts_with("lifecycle_") {
                        event.kind = ActivityKind::Lifecycle;
                    }
                    if !supported_activity_schema(event.schema_version)
                        || !event.has_consistent_payload()
                        || activity_kinds
                            .get(&event.activity_id)
                            .is_some_and(|kind| *kind != event.kind)
                    {
                        record_malformed(&mut log.diagnostics, offset);
                    } else {
                        activity_kinds.insert(event.activity_id.clone(), event.kind);
                        log.events.push(event);
                    }
                } else if let Ok(row) = serde_json::from_slice::<DiagnosticRow>(line) {
                    apply_diagnostic(&mut log.diagnostics, row);
                } else {
                    record_malformed(&mut log.diagnostics, offset);
                }
            }
            offset = offset.saturating_add(raw_line.len() as u64);
        }
        log.diagnostics.duplicate_terminal_states = duplicate_terminal_count(&log.events);
        Ok(log)
    }
}

fn supported_activity_schema(version: u32) -> bool {
    (MIN_ACTIVITY_SCHEMA_VERSION..=ACTIVITY_SCHEMA_VERSION).contains(&version)
}

fn lock_with_timeout(
    file: &File,
    timeout_ms: u64,
    kind: LockKind,
) -> Result<LockGuard<'_>, ActivityStoreError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut first_attempt = true;
    loop {
        if !first_attempt && Instant::now() >= deadline {
            return Err(ActivityStoreError::LockTimeout);
        }
        first_attempt = false;
        let attempt = match kind {
            LockKind::Shared => FileExt::try_lock_shared(file),
            LockKind::Exclusive => file.try_lock_exclusive(),
        };
        match attempt {
            Ok(()) => return Ok(LockGuard { file }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(ActivityStoreError::LockTimeout);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ActivityStoreError::LockTimeout);
                }
                thread::sleep(LOCK_RETRY.min(remaining));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn repair_tail(file: &mut File) -> Result<Option<u64>, ActivityStoreError> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(None);
    }

    let start = find_tail_start(file, length)?;
    let tail_length = length.saturating_sub(start);
    if tail_length <= MAX_ACTIVITY_EVENT_BYTES as u64 {
        file.seek(SeekFrom::Start(start))?;
        let mut tail = vec![0_u8; tail_length as usize];
        file.read_exact(&mut tail)?;
        if serde_json::from_slice::<serde_json::Value>(&tail).is_ok() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            return Ok(None);
        }
    }

    file.set_len(start)?;
    Ok(Some(tail_length))
}

fn find_tail_start(file: &mut File, length: u64) -> io::Result<u64> {
    let mut cursor = length;
    let mut buffer = [0_u8; 8 * 1024];
    while cursor > 0 {
        let chunk_len = usize::try_from(cursor.min(buffer.len() as u64)).unwrap_or(buffer.len());
        cursor -= chunk_len as u64;
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut buffer[..chunk_len])?;
        if let Some(index) = buffer[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            return Ok(cursor + index as u64 + 1);
        }
    }
    Ok(0)
}

fn project_snapshot(log: ActivityLog, limits: SnapshotLimits, now_ms: u64) -> ActivitySnapshot {
    let mut groups = HashMap::<String, Vec<&ActivityEvent>>::new();
    for event in &log.events {
        groups
            .entry(event.activity_id.clone())
            .or_default()
            .push(event);
    }
    let mut superseded = HashSet::<String>::new();
    for events in groups.values() {
        superseded.extend(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event.outcome,
                        Some(coding_brain_core::brain_activity::ActivityOutcome::Succeeded)
                    )
                })
                .filter_map(|event| event.supersedes.as_ref().cloned()),
        );
    }

    let mut unresolved_count = 0;
    let mut attention = HashMap::<String, AttentionItem>::new();
    let mut recent = Vec::new();
    for events in groups.into_values() {
        let item = project_activity(&events, limits.interrupted_after_ms, now_ms);
        if item.kind == ActivityKind::Lifecycle {
            continue;
        }
        let resolved = item.outcome.is_some()
            || item.correction.is_some()
            || superseded.contains(item.activity_id.as_str());
        let needs_attention = !resolved
            && (matches!(
                item.state,
                ActivityState::Denied
                    | ActivityState::Abstained
                    | ActivityState::Error
                    | ActivityState::Interrupted
            ) || matches!(
                item.delivery,
                DeliveryState::Unknown | DeliveryState::Failed
            ))
            && !matches!(
                (item.state, item.delivery),
                (ActivityState::Denied, DeliveryState::Delivered)
            );
        let failed_outcome = matches!(
            item.outcome,
            Some(coding_brain_core::brain_activity::ActivityOutcome::Failed)
        );
        if needs_attention || failed_outcome {
            if needs_attention {
                unresolved_count += 1;
            }
            let key = attention_key(&item);
            attention
                .entry(key)
                .and_modify(|existing| {
                    existing.occurrences += 1;
                    if needs_attention {
                        existing.unresolved_occurrences += 1;
                    }
                    let item_rank = activity_rank(&item);
                    let existing_rank = activity_rank(&existing.activity);
                    if item_rank > existing_rank
                        || (item_rank == existing_rank
                            && (item.recorded_at_ms > existing.recorded_at_ms
                                || (item.recorded_at_ms == existing.recorded_at_ms
                                    && item.activity_id < existing.activity_id)))
                    {
                        existing.activity = item.clone();
                    }
                })
                .or_insert(AttentionItem {
                    activity: item,
                    occurrences: 1,
                    unresolved_occurrences: usize::from(needs_attention),
                });
        } else {
            recent.push(item);
        }
    }

    let mut attention = attention.into_values().collect::<Vec<_>>();
    attention.sort_by(|left, right| {
        attention_rank(right)
            .cmp(&attention_rank(left))
            .then_with(|| right.recorded_at_ms.cmp(&left.recorded_at_ms))
            .then_with(|| left.activity_id.cmp(&right.activity_id))
    });
    attention.truncate(limits.attention);
    recent.sort_by(|left, right| {
        right
            .recorded_at_ms
            .cmp(&left.recorded_at_ms)
            .then_with(|| left.activity_id.cmp(&right.activity_id))
    });
    recent.truncate(limits.recent);
    ActivitySnapshot {
        attention,
        recent,
        unresolved_count,
        diagnostics: log.diagnostics,
    }
}

fn project_activity(events: &[&ActivityEvent], stale_after_ms: u64, now_ms: u64) -> ActivityItem {
    let first = events[0];
    let mut source = first;
    let mut terminal = None;
    let mut latest_state = first.state;
    let mut latest_at = first.recorded_at_ms;
    let mut delivery = DeliveryState::NotApplicable;
    let mut outcome = None;
    let mut correction = None;
    let mut note = None;
    for event in events {
        if event.state.is_terminal() {
            if terminal.is_some() {
                continue;
            }
            terminal = Some(event.state);
            source = event;
        }
        if event.recorded_at_ms >= latest_at {
            latest_at = event.recorded_at_ms;
            latest_state = event.state;
        }
        match event.state {
            ActivityState::Delivered if delivery != DeliveryState::Failed => {
                delivery = DeliveryState::Delivered;
            }
            ActivityState::DeliveryFailed => delivery = DeliveryState::Failed,
            _ => {}
        }
        match event.outcome {
            Some(ActivityOutcome::Completed)
                if outcome.is_some_and(|existing| existing != ActivityOutcome::Completed) => {}
            Some(next) => outcome = Some(next),
            None => {}
        }
        if event.correction.is_some() {
            correction = event.correction;
            note.clone_from(&event.note);
        }
    }
    let mut state = terminal.unwrap_or(latest_state);
    if terminal.is_none()
        && matches!(state, ActivityState::Observed | ActivityState::Evaluating)
        && now_ms.saturating_sub(latest_at) > stale_after_ms
    {
        state = ActivityState::Interrupted;
    }
    if matches!(state, ActivityState::Allowed | ActivityState::Denied)
        && delivery == DeliveryState::NotApplicable
    {
        delivery = DeliveryState::Unknown;
    }
    let normalized_command = source.normalized_command.clone().or_else(|| {
        events
            .iter()
            .find_map(|event| event.normalized_command.clone())
    });

    ActivityItem {
        activity_id: source.activity_id.clone(),
        kind: source.kind,
        recorded_at_ms: latest_at,
        project: source.project.clone(),
        session: source.session.clone(),
        state,
        delivery,
        tool: source.tool.clone(),
        normalized_command,
        fingerprint: source.fingerprint.clone(),
        rule_id: source.rule_id.clone(),
        confidence: source.confidence,
        threshold: source.threshold,
        reasoning: source.reasoning.clone(),
        decision_id: source.decision_id.clone(),
        outcome,
        correction,
        note,
        tool_execution_confirmed: outcome.is_some(),
    }
}

fn attention_key(item: &ActivityItem) -> String {
    format!(
        "{:?}\u{1f}{}\u{1f}{}",
        item.project.project_id,
        item.rule_id.as_deref().unwrap_or(""),
        item.fingerprint
            .as_deref()
            .or(item.normalized_command.as_deref())
            .unwrap_or(&item.activity_id)
    )
}

fn attention_rank(item: &AttentionItem) -> u8 {
    activity_rank(&item.activity)
}

fn activity_rank(item: &ActivityItem) -> u8 {
    if item.delivery == DeliveryState::Failed
        || matches!(
            item.outcome,
            Some(coding_brain_core::brain_activity::ActivityOutcome::Failed)
        )
    {
        5
    } else {
        match item.state {
            ActivityState::Denied => 4,
            ActivityState::Error => 3,
            ActivityState::Interrupted => 2,
            ActivityState::Abstained => 1,
            _ => 0,
        }
    }
}

fn duplicate_terminal_count(events: &[ActivityEvent]) -> usize {
    let mut seen = HashSet::new();
    events
        .iter()
        .filter(|event| event.state.is_terminal())
        .filter(|event| !seen.insert(event.activity_id.as_str()))
        .count()
}

fn write_diagnostic(
    writer: &mut impl Write,
    diagnostic: StoreDiagnostic,
) -> Result<(), ActivityStoreError> {
    serde_json::to_writer(
        &mut *writer,
        &DiagnosticRow {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            diagnostic,
        },
    )?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn apply_diagnostic(diagnostics: &mut ActivityDiagnostics, row: DiagnosticRow) {
    if !supported_activity_schema(row.schema_version) {
        diagnostics.malformed_rows += 1;
        return;
    }
    match row.diagnostic {
        StoreDiagnostic::TruncatedTail { discarded_bytes } => {
            diagnostics.truncated_tails += 1;
            diagnostics.discarded_tail_bytes = diagnostics
                .discarded_tail_bytes
                .saturating_add(discarded_bytes);
        }
        StoreDiagnostic::MalformedRows { count } => {
            diagnostics.malformed_rows = diagnostics.malformed_rows.saturating_add(count);
        }
    }
}

fn record_malformed(diagnostics: &mut ActivityDiagnostics, offset: u64) {
    diagnostics.malformed_rows += 1;
    if diagnostics.malformed_offsets.len() < MAX_DIAGNOSTIC_OFFSETS {
        diagnostics.malformed_offsets.push(offset);
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn set_dir_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityOutcome, CorrectionDisposition, DeliveryState,
        ProjectEvidence,
    };
    use coding_brain_core::project::ProjectId;

    use super::*;

    fn fixture_store() -> (tempfile::TempDir, ActivityStore) {
        let root = tempfile::tempdir().unwrap();
        let store = ActivityStore::at(root.path().join("activity.jsonl"));
        (root, store)
    }

    fn event_at(activity_id: &str, state: ActivityState, recorded_at_ms: u64) -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: activity_id.into(),
            recorded_at_ms,
            project: ProjectEvidence {
                project_id: ProjectId::Temporary("project".into()),
                cwd: PathBuf::from("/work/project"),
                label: Some("project".into()),
            },
            session: None,
            state,
            tool: Some("Bash".into()),
            normalized_command: Some(format!("command-{activity_id}")),
            fingerprint: Some(format!("fingerprint-{activity_id}")),
            rule_id: Some("destructive".into()),
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("reason".into()),
            decision_id: Some(format!("decision-{activity_id}")),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn event(activity_id: &str, state: ActivityState) -> ActivityEvent {
        event_at(activity_id, state, 100)
    }

    #[test]
    fn mixed_v1_v2_rows_read_and_compact_without_version_rewrite() {
        let (temp, store) = fixture_store();
        let store = store.with_limits(ActivityLimits {
            compact_at_bytes: 1,
            retained_lifecycles: 10,
            ..ActivityLimits::default()
        });
        let mut v1 = event_at("v1", ActivityState::Allowed, 1);
        v1.schema_version = 1;
        fs::write(
            temp.path().join("activity.jsonl"),
            format!("{}\n", serde_json::to_string(&v1).unwrap()),
        )
        .unwrap();
        store
            .append(event_at("v2", ActivityState::Allowed, 2))
            .unwrap();

        let versions = store
            .read()
            .unwrap()
            .events()
            .iter()
            .map(|event| event.schema_version)
            .collect::<Vec<_>>();
        assert_eq!(versions, [1, 2]);
        assert!(store.compact_if_needed().unwrap());
        let versions = store
            .read()
            .unwrap()
            .events()
            .iter()
            .map(|event| event.schema_version)
            .collect::<Vec<_>>();
        assert_eq!(versions, [1, 2]);
    }

    #[test]
    fn completed_is_neutral_and_does_not_supersede() {
        let (_, store) = fixture_store();
        store
            .append(event_at("denied", ActivityState::Denied, 1))
            .unwrap();
        store
            .append(event_at("denied", ActivityState::DeliveryFailed, 2))
            .unwrap();
        let mut completed = event_at("completed", ActivityState::Allowed, 3);
        completed.state = ActivityState::Outcome;
        completed.outcome = Some(ActivityOutcome::Completed);
        completed.supersedes = Some("denied".into());
        store.append(completed).unwrap();
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert!(
            snapshot
                .attention
                .iter()
                .any(|item| item.activity_id == "denied")
        );
    }

    #[test]
    fn completed_supersession_stays_neutral_when_same_activity_later_succeeds() {
        let (_, store) = fixture_store();
        store
            .append(event_at("denied", ActivityState::Denied, 1))
            .unwrap();

        let mut completed = event_at("later", ActivityState::Outcome, 2);
        completed.outcome = Some(ActivityOutcome::Completed);
        completed.supersedes = Some("denied".into());
        store.append(completed).unwrap();

        let mut succeeded = event_at("later", ActivityState::Outcome, 3);
        succeeded.outcome = Some(ActivityOutcome::Succeeded);
        store.append(succeeded).unwrap();

        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert!(
            snapshot
                .attention
                .iter()
                .any(|item| item.activity_id == "denied")
        );
    }

    #[test]
    fn candidate_losslessness_keeps_earlier_command_available_for_projection() {
        let observed = event_at("activity-1", ActivityState::Observed, 1);
        let mut terminal = event_at("activity-1", ActivityState::Allowed, 2);
        terminal.normalized_command = None;
        let item = project_activity(&[&observed, &terminal], 30_000, 2);

        assert_eq!(
            item.normalized_command.as_deref(),
            observed.normalized_command.as_deref()
        );
    }

    #[test]
    fn equivalent_evidence_completed_never_replaces_explicit_failed_outcome() {
        for outcomes in [
            [ActivityOutcome::Failed, ActivityOutcome::Completed],
            [ActivityOutcome::Completed, ActivityOutcome::Failed],
        ] {
            let allowed = event_at("activity-1", ActivityState::Allowed, 1);
            let mut first = event_at("activity-1", ActivityState::Outcome, 2);
            first.outcome = Some(outcomes[0]);
            let mut second = event_at("activity-1", ActivityState::Outcome, 3);
            second.outcome = Some(outcomes[1]);

            let item = project_activity(&[&allowed, &first, &second], 30_000, 3);

            assert_eq!(item.outcome, Some(ActivityOutcome::Failed));
        }
    }

    #[test]
    fn v1_diagnostic_rows_remain_readable() {
        let (temp, store) = fixture_store();
        fs::write(
            temp.path().join("activity.jsonl"),
            b"{\"schema_version\":1,\"diagnostic\":{\"kind\":\"malformed_rows\",\"count\":2}}\n",
        )
        .unwrap();
        assert_eq!(store.read().unwrap().diagnostics().malformed_rows, 2);
    }

    #[test]
    fn current_writer_rejects_v1_and_reader_diagnoses_v3() {
        let (temp, store) = fixture_store();
        let mut v1 = event_at("v1", ActivityState::Allowed, 1);
        v1.schema_version = 1;
        assert!(matches!(
            store.append(v1),
            Err(ActivityStoreError::UnsupportedSchema(1))
        ));

        let mut v3 = event_at("v3", ActivityState::Allowed, 3);
        v3.schema_version = 3;
        fs::write(
            temp.path().join("activity.jsonl"),
            format!("{}\n", serde_json::to_string(&v3).unwrap()),
        )
        .unwrap();
        assert_eq!(store.read().unwrap().diagnostics().malformed_rows, 1);
    }

    #[test]
    fn v1_decision_and_v2_outcome_project_and_compact_together() {
        let (temp, store) = fixture_store();
        let store = store.with_limits(ActivityLimits {
            compact_at_bytes: 1,
            retained_lifecycles: 10,
            ..ActivityLimits::default()
        });
        let mut decision = event_at("mixed", ActivityState::Allowed, 1);
        decision.schema_version = 1;
        fs::write(
            temp.path().join("activity.jsonl"),
            format!("{}\n", serde_json::to_string(&decision).unwrap()),
        )
        .unwrap();
        let mut outcome = event_at("mixed", ActivityState::Outcome, 2);
        outcome.outcome = Some(ActivityOutcome::Completed);
        store.append(outcome).unwrap();
        assert_eq!(
            store.snapshot(SnapshotLimits::default()).unwrap().recent[0].outcome,
            Some(ActivityOutcome::Completed)
        );
        assert!(store.compact_if_needed().unwrap());
        assert_eq!(
            store
                .read()
                .unwrap()
                .events()
                .iter()
                .map(|event| event.schema_version)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn append_from_snapshot_serializes_concurrent_idempotency_checks() {
        let (_, store) = fixture_store();
        store
            .append(event_at("target", ActivityState::Allowed, 1))
            .unwrap();
        let store = std::sync::Arc::new(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|index| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let marker = event_at(
                        &format!("marker-{index}"),
                        ActivityState::Observed,
                        index + 2,
                    );
                    let mut outcome = event_at("target", ActivityState::Outcome, index + 4);
                    outcome.outcome = Some(ActivityOutcome::Completed);
                    barrier.wait();
                    store
                        .append_from_snapshot(|log| {
                            let mut rows = vec![marker];
                            if !log.events().iter().any(|event| {
                                event.activity_id == "target"
                                    && event.state == ActivityState::Outcome
                            }) {
                                rows.push(outcome);
                            }
                            rows
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let events = store.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.activity_id.starts_with("marker-"))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            1
        );
    }

    #[test]
    fn legacy_lifecycle_ids_are_normalized_on_read() {
        let (root, store) = fixture_store();
        let mut event = event("lifecycle_1", ActivityState::Abstained);
        event.normalized_command = None;
        event.fingerprint = None;
        event.rule_id = None;
        event.confidence = None;
        event.threshold = None;
        event.reasoning = None;
        event.decision_id = None;
        event.tool = Some("SessionStart".into());
        let mut legacy = serde_json::to_value(event).unwrap();
        legacy.as_object_mut().unwrap().remove("kind");
        fs::write(root.path().join("activity.jsonl"), format!("{legacy}\n")).unwrap();
        assert_eq!(
            store.read().unwrap().events()[0].kind,
            ActivityKind::Lifecycle
        );
    }

    #[test]
    fn explicit_kind_is_preserved_for_lifecycle_prefixed_activity_id() {
        let (_root, store) = fixture_store();
        store
            .append(event("lifecycle_explicit_decision", ActivityState::Denied))
            .unwrap();

        let log = store.read().unwrap();
        assert_eq!(log.events().len(), 1);
        assert_eq!(log.events()[0].kind, ActivityKind::Decision);
    }

    #[test]
    fn mixed_activity_kinds_are_diagnosed() {
        let (_root, store) = fixture_store();
        let first = event("same", ActivityState::Denied);
        let mut conflicting = first.clone();
        conflicting.kind = ActivityKind::Diagnostic;
        conflicting.state = ActivityState::Error;
        conflicting.decision_id = None;
        store.append(first).unwrap();
        store.append(conflicting).unwrap();
        let log = store.read().unwrap();
        assert_eq!(log.events().len(), 1);
        assert_eq!(log.diagnostics().malformed_rows, 1);
    }

    #[test]
    fn lifecycle_activity_is_audited_but_absent_from_live_snapshot() {
        let (_root, store) = fixture_store();
        let mut lifecycle = event("lifecycle_1", ActivityState::Abstained);
        lifecycle.kind = ActivityKind::Lifecycle;
        lifecycle.normalized_command = None;
        lifecycle.fingerprint = None;
        lifecycle.rule_id = None;
        lifecycle.confidence = None;
        lifecycle.threshold = None;
        lifecycle.reasoning = None;
        lifecycle.decision_id = None;
        lifecycle.tool = Some("SessionStart".into());
        store.append(lifecycle).unwrap();
        store
            .append(event("decision-1", ActivityState::Abstained))
            .unwrap();
        let mut diagnostic = event("orphan_1", ActivityState::Error);
        diagnostic.kind = ActivityKind::Diagnostic;
        diagnostic.decision_id = None;
        store.append(diagnostic).unwrap();

        assert_eq!(store.read().unwrap().events().len(), 3);
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention.len(), 2);
        assert_eq!(snapshot.unresolved_count, 2);
        assert!(snapshot.recent.is_empty());
        assert!(
            snapshot
                .attention
                .iter()
                .all(|item| item.kind != ActivityKind::Lifecycle)
        );
    }

    #[test]
    fn first_terminal_state_wins_and_late_terminal_is_diagnostic() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Observed)).unwrap();
        store
            .append(event("a1", ActivityState::Evaluating))
            .unwrap();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        store
            .append(event_at("a1", ActivityState::Allowed, 1_000))
            .unwrap();

        let log = store.read().unwrap();
        assert_eq!(
            log.activity("a1").unwrap().terminal_state(),
            ActivityState::Denied
        );
        assert_eq!(log.diagnostics().duplicate_terminal_states, 1);
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention[0].recorded_at_ms, 100);
    }

    #[test]
    fn inconsistent_state_payload_is_rejected_and_diagnosed_on_read() {
        let (root, store) = fixture_store();
        let mut inconsistent = event("a1", ActivityState::Allowed);
        inconsistent.outcome = Some(ActivityOutcome::Succeeded);
        assert!(store.append(inconsistent.clone()).is_err());

        let mut row = serde_json::to_vec(&inconsistent).unwrap();
        row.push(b'\n');
        fs::write(root.path().join("activity.jsonl"), row).unwrap();
        store.append(event("a2", ActivityState::Denied)).unwrap();
        let log = store.read().unwrap();
        assert_eq!(log.events().len(), 1);
        assert_eq!(log.diagnostics().malformed_rows, 1);
    }

    #[test]
    fn stale_evaluating_projects_as_interrupted_without_rewriting_source() {
        let (root, store) = fixture_store();
        let store = store.with_clock(1_000);
        store
            .append(event_at("a1", ActivityState::Observed, 100))
            .unwrap();
        store
            .append(event_at("a1", ActivityState::Evaluating, 101))
            .unwrap();
        let snapshot = store
            .snapshot(SnapshotLimits {
                interrupted_after_ms: 100,
                ..SnapshotLimits::default()
            })
            .unwrap();
        assert_eq!(snapshot.attention[0].state, ActivityState::Interrupted);
        assert_eq!(store.read().unwrap().events().len(), 2);
        drop(root);
    }

    #[test]
    fn committed_decision_without_delivery_evidence_projects_unknown() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Allowed)).unwrap();
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention[0].delivery, DeliveryState::Unknown);
        assert!(!snapshot.attention[0].tool_execution_confirmed);
    }

    #[test]
    fn delivery_and_outcome_evidence_are_distinct() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Allowed)).unwrap();
        store.append(event("a1", ActivityState::Delivered)).unwrap();
        let delivered = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(delivered.recent[0].delivery, DeliveryState::Delivered);
        assert!(!delivered.recent[0].tool_execution_confirmed);

        let mut outcome = event_at("a1", ActivityState::Outcome, 101);
        outcome.outcome = Some(ActivityOutcome::Succeeded);
        store.append(outcome).unwrap();
        let completed = store.snapshot(SnapshotLimits::default()).unwrap();
        assert!(completed.recent[0].tool_execution_confirmed);
    }

    #[test]
    fn denial_delivery_controls_attention() {
        let (_root, delivered_store) = fixture_store();
        delivered_store
            .append(event("delivered", ActivityState::Denied))
            .unwrap();
        delivered_store
            .append(event_at("delivered", ActivityState::Delivered, 101))
            .unwrap();

        let delivered = delivered_store.snapshot(SnapshotLimits::default()).unwrap();
        assert!(delivered.attention.is_empty());
        assert_eq!(delivered.unresolved_count, 0);
        assert_eq!(delivered.recent.len(), 1);
        assert_eq!(delivered.recent[0].state, ActivityState::Denied);
        assert_eq!(delivered.recent[0].delivery, DeliveryState::Delivered);

        let (_root, unknown_store) = fixture_store();
        unknown_store
            .append(event("unknown", ActivityState::Denied))
            .unwrap();
        let unknown = unknown_store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(unknown.attention.len(), 1);
        assert_eq!(unknown.attention[0].delivery, DeliveryState::Unknown);
        assert_eq!(unknown.unresolved_count, 1);
    }

    #[test]
    fn delivery_failure_needs_attention() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        store
            .append(event_at("a1", ActivityState::DeliveryFailed, 101))
            .unwrap();
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention[0].delivery, DeliveryState::Failed);
    }

    #[test]
    fn repeated_attention_collapses_but_source_events_remain() {
        let (_root, store) = fixture_store();
        for id in ["a1", "a2"] {
            let mut denial = event(id, ActivityState::Denied);
            denial.normalized_command = Some("rm -rf build".into());
            denial.fingerprint = Some("same-command".into());
            store.append(denial).unwrap();
        }
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention.len(), 1);
        assert_eq!(snapshot.attention[0].occurrences, 2);
        assert_eq!(snapshot.attention[0].unresolved_occurrences, 2);
        assert_eq!(store.read().unwrap().complete_lifecycles(), 2);
    }

    #[test]
    fn collapsed_attention_preserves_highest_risk_evidence() {
        let (_root, store) = fixture_store();
        let mut allowed = event_at("a1", ActivityState::Allowed, 10);
        allowed.fingerprint = Some("shared".into());
        store.append(allowed).unwrap();
        let mut failed_delivery = event_at("a1", ActivityState::DeliveryFailed, 11);
        failed_delivery.fingerprint = Some("shared".into());
        store.append(failed_delivery).unwrap();
        let mut abstained = event_at("a2", ActivityState::Abstained, 20);
        abstained.fingerprint = Some("shared".into());
        store.append(abstained).unwrap();

        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention.len(), 1);
        assert_eq!(snapshot.attention[0].occurrences, 2);
        assert_eq!(snapshot.attention[0].activity_id, "a1");
        assert_eq!(snapshot.attention[0].delivery, DeliveryState::Failed);
    }

    #[test]
    fn corrections_and_successful_supersession_resolve_attention() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        let mut correction = event_at("a1", ActivityState::Correction, 101);
        correction.correction = Some(CorrectionDisposition::BrainRight);
        store.append(correction).unwrap();
        assert!(
            store
                .snapshot(SnapshotLimits::default())
                .unwrap()
                .attention
                .is_empty()
        );

        store.append(event("a2", ActivityState::Denied)).unwrap();
        let mut later = event_at("a3", ActivityState::Outcome, 102);
        later.supersedes = Some("a2".into());
        later.outcome = Some(ActivityOutcome::Succeeded);
        store.append(later).unwrap();
        assert!(
            store
                .snapshot(SnapshotLimits::default())
                .unwrap()
                .attention
                .iter()
                .all(|item| item.activity_id != "a2")
        );
    }

    #[test]
    fn unsuccessful_supersession_does_not_resolve_attention() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        let mut later = event_at("a2", ActivityState::Error, 102);
        later.supersedes = Some("a1".into());
        store.append(later).unwrap();
        assert!(
            store
                .snapshot(SnapshotLimits::default())
                .unwrap()
                .attention
                .iter()
                .any(|item| item.activity_id == "a1")
        );
    }

    #[test]
    fn resolved_failed_outcome_is_attention_but_not_unresolved() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Allowed)).unwrap();
        store.append(event("a1", ActivityState::Delivered)).unwrap();
        let mut outcome = event_at("a1", ActivityState::Outcome, 101);
        outcome.outcome = Some(ActivityOutcome::Failed);
        store.append(outcome).unwrap();
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention.len(), 1);
        assert_eq!(snapshot.unresolved_count, 0);
        assert_eq!(snapshot.attention[0].unresolved_occurrences, 0);
    }

    #[test]
    fn malformed_rows_are_diagnosed_and_valid_rows_continue() {
        let (root, store) = fixture_store();
        fs::write(root.path().join("activity.jsonl"), b"{bad}\n").unwrap();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        let log = store.read().unwrap();
        assert_eq!(log.events().len(), 1);
        assert_eq!(log.diagnostics().malformed_rows, 1);
        assert_eq!(log.diagnostics().malformed_offsets, vec![0]);
    }

    #[test]
    fn append_completes_valid_unterminated_json() {
        let (root, store) = fixture_store();
        let first = serde_json::to_vec(&event("a1", ActivityState::Denied)).unwrap();
        fs::write(root.path().join("activity.jsonl"), first).unwrap();
        store.append(event("a2", ActivityState::Denied)).unwrap();
        assert_eq!(store.read().unwrap().events().len(), 2);
    }

    #[test]
    fn append_completes_large_valid_unterminated_json_within_row_limit() {
        let (root, store) = fixture_store();
        let mut large = event("large", ActivityState::Denied);
        large.project.cwd = PathBuf::from(format!("/{}", "p".repeat(20_000)));
        large.session = Some(coding_brain_core::brain_activity::SessionTarget {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            session_id: "session".into(),
            turn_id: None,
            tool_use_id: None,
            project_id: ProjectId::Temporary("project".into()),
            cwd: PathBuf::from(format!("/{}", "s".repeat(20_000))),
            provider_hints: Vec::new(),
        });
        store.append(large).unwrap();
        let path = root.path().join("activity.jsonl");
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 32 * 1024);
        assert!(bytes.len() - 1 <= MAX_ACTIVITY_EVENT_BYTES);
        fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();

        store.append(event("after", ActivityState::Denied)).unwrap();
        assert_eq!(store.read().unwrap().events().len(), 2);
    }

    #[test]
    fn append_repairs_invalid_crash_tail_without_copying_it() {
        let (root, store) = fixture_store();
        let mut contents = serde_json::to_vec(&event("a1", ActivityState::Denied)).unwrap();
        contents.push(b'\n');
        contents.extend_from_slice(b"{\"secret\":\"never-copy-me");
        fs::write(root.path().join("activity.jsonl"), contents).unwrap();
        store.append(event("a2", ActivityState::Denied)).unwrap();
        let raw = fs::read_to_string(root.path().join("activity.jsonl")).unwrap();
        assert!(!raw.contains("never-copy-me"));
        let log = store.read().unwrap();
        assert_eq!(log.events().len(), 2);
        assert_eq!(log.diagnostics().truncated_tails, 1);
        assert!(log.diagnostics().discarded_tail_bytes > 0);
    }

    #[test]
    fn snapshot_limits_attention_and_reports_overflow() {
        let (_root, store) = fixture_store();
        for index in 0..105 {
            store
                .append(event_at(&format!("a{index}"), ActivityState::Denied, index))
                .unwrap();
        }
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention.len(), 100);
        assert_eq!(snapshot.unresolved_count, 105);
        assert_eq!(
            snapshot
                .attention
                .iter()
                .map(|item| item.unresolved_occurrences)
                .sum::<usize>(),
            100
        );
    }

    #[test]
    fn snapshot_limits_recent_and_ranks_safety_before_recency() {
        let (_root, store) = fixture_store();
        for index in 0..105 {
            let id = format!("recent-{index}");
            store
                .append(event_at(&id, ActivityState::Allowed, index))
                .unwrap();
            store
                .append(event_at(&id, ActivityState::Delivered, index))
                .unwrap();
        }
        store
            .append(event_at("older-denial", ActivityState::Denied, 200))
            .unwrap();
        store
            .append(event_at("newer-abstention", ActivityState::Abstained, 300))
            .unwrap();
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.recent.len(), 100);
        assert_eq!(snapshot.attention[0].state, ActivityState::Denied);
    }

    #[test]
    fn equal_rank_and_timestamp_rows_have_stable_id_order() {
        let (_root, store) = fixture_store();
        for id in ["b", "a"] {
            store
                .append(event_at(id, ActivityState::Denied, 100))
                .unwrap();
        }
        for id in ["d", "c"] {
            store
                .append(event_at(id, ActivityState::Allowed, 100))
                .unwrap();
            store
                .append(event_at(id, ActivityState::Delivered, 100))
                .unwrap();
        }
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(
            snapshot
                .attention
                .iter()
                .map(|item| item.activity_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            snapshot
                .recent
                .iter()
                .map(|item| item.activity_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "d"]
        );
    }

    #[test]
    fn lock_wait_is_bounded_and_busy_compaction_skips() {
        let (_root, store) = fixture_store();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&store.lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();

        let started = Instant::now();
        assert!(matches!(
            store.append(event("a2", ActivityState::Denied)),
            Err(ActivityStoreError::LockTimeout)
        ));
        assert!(
            started.elapsed()
                < Duration::from_millis(store.limits.lock_timeout_ms.saturating_add(25))
        );
        assert!(
            !store
                .clone()
                .with_limits(ActivityLimits {
                    compact_at_bytes: 1,
                    ..ActivityLimits::default()
                })
                .compact_if_needed()
                .unwrap()
        );
        FileExt::unlock(&lock).unwrap();
    }

    #[test]
    fn late_duplicate_terminal_does_not_change_compaction_recency() {
        let (_root, store) = fixture_store();
        let store = store.with_limits(ActivityLimits {
            compact_at_bytes: 1,
            retained_lifecycles: 1,
            ..ActivityLimits::default()
        });
        store
            .append(event_at("older", ActivityState::Denied, 1))
            .unwrap();
        store
            .append(event_at("newer", ActivityState::Denied, 2))
            .unwrap();
        store
            .append(event_at("older", ActivityState::Allowed, 100))
            .unwrap();
        assert!(store.compact_if_needed().unwrap());
        let ids = store
            .read()
            .unwrap()
            .events()
            .iter()
            .map(|event| event.activity_id.clone())
            .collect::<HashSet<_>>();
        assert!(ids.contains("newer"));
        assert!(!ids.contains("older"));
    }

    #[test]
    fn compaction_evicts_stale_incomplete_lifecycles_but_keeps_active_one() {
        let (_root, store) = fixture_store();
        let now = 100_000_000;
        let store = store.with_clock(now).with_limits(ActivityLimits {
            compact_at_bytes: 1,
            ..ActivityLimits::default()
        });
        for index in 0..=MAX_RETAINED_INTERRUPTED_LIFECYCLES {
            store
                .append(event_at(
                    &format!("stale-{index}"),
                    ActivityState::Evaluating,
                    index as u64,
                ))
                .unwrap();
        }
        store
            .append(event_at("active", ActivityState::Evaluating, now - 1))
            .unwrap();
        assert!(store.compact_if_needed().unwrap());
        let log = store.read().unwrap();
        assert!(
            log.events()
                .iter()
                .any(|event| event.activity_id == "active")
        );
        assert!(
            !log.events()
                .iter()
                .any(|event| event.activity_id == "stale-0")
        );
    }

    #[test]
    fn compaction_preserves_all_fresh_incomplete_lifecycles() {
        let (_root, store) = fixture_store();
        let now = 100_000_000;
        let store = store.with_clock(now).with_limits(ActivityLimits {
            compact_at_bytes: 1,
            ..ActivityLimits::default()
        });
        for index in 0..300 {
            store
                .append(event_at(
                    &format!("active-{index}"),
                    ActivityState::Evaluating,
                    now - index,
                ))
                .unwrap();
        }
        assert!(store.compact_if_needed().unwrap());
        assert_eq!(store.read().unwrap().events().len(), 300);
    }

    #[test]
    fn oversized_normalized_event_is_rejected() {
        let (_root, store) = fixture_store();
        let mut oversized = event("a1", ActivityState::Denied);
        oversized.project.cwd = PathBuf::from(format!("/{}", "c".repeat(40_000)));
        oversized.reasoning = Some("r".repeat(5_000));
        oversized.session = Some(coding_brain_core::brain_activity::SessionTarget {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            session_id: "s".repeat(5_000),
            turn_id: None,
            tool_use_id: None,
            project_id: ProjectId::Temporary("p".repeat(5_000)),
            cwd: PathBuf::from(format!("/{}", "d".repeat(40_000))),
            provider_hints: Vec::new(),
        });
        assert!(store.append(oversized).is_err());
        assert!(store.read().unwrap().events().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn activity_storage_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (root, store) = fixture_store();
        store.append(event("a1", ActivityState::Denied)).unwrap();
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.path().join("activity.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.path().join("activity.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
