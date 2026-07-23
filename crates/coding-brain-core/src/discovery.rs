use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::codex_transcript::{CodexEvent, parse_line};
use crate::process::{ProcessSnapshot, ProcessSnapshotEntry, capture_process_snapshot};
use crate::provider::AgentProvider;
use crate::session::{AgentSession, RawAgentSession, SessionIdentityProvenance, SessionStatus};

pub mod antigravity;
pub mod claude;

const TRANSCRIPT_INDEX_TTL: Duration = Duration::from_secs(10);

fn sessions_dir() -> PathBuf {
    codex_home().join("sessions")
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEXCTL_CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".codex"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn projects_dir() -> PathBuf {
    sessions_dir()
}

pub fn scan_sessions() -> Vec<AgentSession> {
    scan_sessions_with_state(&mut TranscriptAssignmentState::default())
}

pub fn scan_sessions_with_state(state: &mut TranscriptAssignmentState) -> Vec<AgentSession> {
    let snapshot = capture_process_snapshot();
    scan_codex_sessions_from_snapshot(&snapshot, state)
}

#[derive(Debug, Default)]
pub struct ProviderDiscoveryState {
    pub transcript_assignments: TranscriptAssignmentState,
    pub claude_inventory: claude::ClaudeInventoryCache,
}

pub fn scan_agent_sessions_with_state(state: &mut ProviderDiscoveryState) -> Vec<AgentSession> {
    scan_agent_sessions_with_runners(
        state,
        Instant::now(),
        capture_process_snapshot,
        claude::run_inventory_command,
    )
}

fn scan_agent_sessions_with_runners<P, C>(
    state: &mut ProviderDiscoveryState,
    now: Instant,
    process_runner: P,
    claude_runner: C,
) -> Vec<AgentSession>
where
    P: FnOnce() -> ProcessSnapshot,
    C: FnOnce(Duration, usize) -> Result<Vec<u8>, claude::InventoryError>,
{
    let snapshot = process_runner();
    let inventory = claude::inventory_with_runner(&mut state.claude_inventory, now, claude_runner);
    let stale_inventory = state.claude_inventory.last_error.is_some();

    let mut sessions =
        scan_codex_sessions_from_snapshot(&snapshot, &mut state.transcript_assignments);
    sessions.extend(claude::sessions_from_inventory(
        &inventory,
        stale_inventory,
        snapshot.succeeded,
        &snapshot.entries,
    ));
    sessions.extend(antigravity::sessions_from_processes(&snapshot.entries));
    sessions.sort_by_key(|session| Reverse(session.started_at));
    sessions
}

fn scan_codex_sessions_from_snapshot(
    snapshot: &ProcessSnapshot,
    state: &mut TranscriptAssignmentState,
) -> Vec<AgentSession> {
    let processes = snapshot
        .entries
        .iter()
        .filter(|process| process.has_executable_basename(&["codex", ".codex-wrapped"]))
        .map(live_codex_process)
        .collect::<Vec<_>>();
    if processes.is_empty() {
        clear_transcript_assignments(state);
        return Vec::new();
    }

    sessions_from_discovered_processes(processes, state)
}

fn clear_transcript_assignments(state: &mut TranscriptAssignmentState) {
    state.retained.clear();
    state.transitions.clear();
    state.unmatched_index_generations.clear();
}

fn live_codex_process(process: &ProcessSnapshotEntry) -> LiveCodexProcess {
    LiveCodexProcess {
        pid: process.pid,
        cwd: process.cwd.to_string_lossy().into_owned(),
        started_at: process.started_at,
        start_identity: process.start_identity,
        tty: process.tty.clone(),
        cpu_percent: process.cpu_percent,
        mem_mb: process.mem_mb,
        command_args: args_after_executable(&process.args, &["codex", ".codex-wrapped"]),
    }
}

fn session_from_provider_process(
    provider: AgentProvider,
    process: &ProcessSnapshotEntry,
) -> AgentSession {
    let mut session = AgentSession::from_raw(RawAgentSession {
        provider,
        pid: process.pid,
        process_start_identity: Some(process.start_identity),
        session_id: process_session_id(process),
        cwd: process.cwd.to_string_lossy().into_owned(),
        started_at: process.started_at,
    });
    session.identity_provenance = SessionIdentityProvenance::ProcessOnly;
    session.status = SessionStatus::Unknown;
    apply_process_evidence(&mut session, process);
    session
}

fn process_session_id(process: &ProcessSnapshotEntry) -> String {
    format!(
        "process:{}:{}:{}:{}",
        process.pid,
        process.start_identity,
        process.tty.len(),
        process.tty
    )
}

fn apply_process_evidence(session: &mut AgentSession, process: &ProcessSnapshotEntry) {
    session.pid = process.pid;
    session.process_start_identity = Some(process.start_identity);
    session.process_backed = true;
    session.tty = process.tty.clone();
    session.cpu_percent = process.cpu_percent;
    session.mem_mb = process.mem_mb;
    session.command_args = process.args.clone();
}

#[derive(Debug, Clone)]
struct LiveCodexProcess {
    pid: u32,
    cwd: String,
    started_at: u64,
    start_identity: u64,
    tty: String,
    cpu_percent: f32,
    mem_mb: f64,
    command_args: String,
}

#[derive(Debug, Clone)]
pub struct CodexTranscriptSummary {
    pub session_id: String,
    pub cwd: String,
    pub path: PathBuf,
    pub started_at_ms: u64,
    pub mtime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetainedTranscript {
    process_start_identity: u64,
    session_id: String,
    path: PathBuf,
    transcript_started_at_ms: u64,
    transcript_mtime_ms: u64,
    resume_superseded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTranscriptTransition {
    session_id: String,
    path: PathBuf,
    consecutive_uncached_scans: u8,
}

#[derive(Debug, Default)]
pub struct TranscriptAssignmentState {
    retained: HashMap<u32, RetainedTranscript>,
    transitions: HashMap<u32, PendingTranscriptTransition>,
    unmatched_index_generations: HashMap<u32, u64>,
}

struct CachedTranscriptIndex {
    sessions_dir: PathBuf,
    refreshed_at: Instant,
    generation: u64,
    transcripts: Vec<CodexTranscriptSummary>,
}

static TRANSCRIPT_INDEX_CACHE: OnceLock<Mutex<Option<CachedTranscriptIndex>>> = OnceLock::new();

fn transcript_index_cache() -> &'static Mutex<Option<CachedTranscriptIndex>> {
    TRANSCRIPT_INDEX_CACHE.get_or_init(|| Mutex::new(None))
}

fn args_after_executable(args: &str, expected: &[&str]) -> String {
    let mut parts = args.split_whitespace();
    let Some(first) = parts.next() else {
        return String::new();
    };
    let first_path = PathBuf::from(first);
    let first_name = first_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first);
    if expected.contains(&first_name) {
        parts.collect::<Vec<_>>().join(" ")
    } else {
        args.to_string()
    }
}

#[cfg(test)]
fn collect_transcript_summaries() -> Vec<CodexTranscriptSummary> {
    collect_transcript_summaries_with_refresh(false).0
}

fn collect_transcript_summaries_with_refresh(
    force_refresh: bool,
) -> (Vec<CodexTranscriptSummary>, bool, u64) {
    let dir = sessions_dir();
    if let Ok(mut cached) = transcript_index_cache().lock() {
        if !force_refresh && let Some(index) = cached.as_ref() {
            if index.sessions_dir == dir && index.refreshed_at.elapsed() < TRANSCRIPT_INDEX_TTL {
                return (index.transcripts.clone(), false, index.generation);
            }
        }

        let generation = cached
            .as_ref()
            .map_or(1, |index| index.generation.saturating_add(1));
        let transcripts = collect_transcript_summaries_uncached(&dir);
        *cached = Some(CachedTranscriptIndex {
            sessions_dir: dir,
            refreshed_at: Instant::now(),
            generation,
            transcripts: transcripts.clone(),
        });
        return (transcripts, true, generation);
    }

    (collect_transcript_summaries_uncached(&dir), true, 0)
}

fn collect_transcript_summaries_uncached(dir: &PathBuf) -> Vec<CodexTranscriptSummary> {
    let mut paths = Vec::new();
    collect_rollout_jsonls(dir, &mut paths);

    let mut transcripts: Vec<CodexTranscriptSummary> = paths
        .into_iter()
        .filter_map(transcript_summary_from_codex_jsonl)
        .collect();
    transcripts.sort_by_key(|t| Reverse(t.mtime_ms));
    transcripts
}

#[cfg(test)]
fn sessions_from_live_processes(
    processes: Vec<LiveCodexProcess>,
    transcripts: &[CodexTranscriptSummary],
) -> Vec<AgentSession> {
    let assigned = assign_transcripts(&processes, transcripts, &HashMap::new());
    let mut sessions: Vec<AgentSession> = processes
        .into_iter()
        .map(|process| {
            let transcript = assigned.get(&process.pid).copied();
            session_from_live_process(process, transcript)
        })
        .collect();
    sessions.sort_by_key(|s| Reverse(s.started_at));
    sessions
}

fn sessions_from_discovered_processes(
    processes: Vec<LiveCodexProcess>,
    state: &mut TranscriptAssignmentState,
) -> Vec<AgentSession> {
    let initializing_assignments = state.retained.is_empty();
    let had_pending_transition = !state.transitions.is_empty();
    let (mut transcripts, refreshed, mut generation) =
        collect_transcript_summaries_with_refresh(false);
    let mut assigned = assign_transcripts_with_state(&processes, &transcripts, state, refreshed);
    if initializing_assignments && refreshed && !state.retained.is_empty() {
        drop(assigned);
        assigned = assign_transcripts_with_state(&processes, &transcripts, state, true);
    }
    let unmatched: Vec<u32> = processes
        .iter()
        .filter(|process| !assigned.contains_key(&process.pid))
        .map(|process| process.pid)
        .collect();
    let has_new_unmatched = unmatched
        .iter()
        .any(|pid| state.unmatched_index_generations.get(pid).copied() != Some(generation));
    let needs_uncached = !refreshed && (has_new_unmatched || had_pending_transition);
    if needs_uncached {
        drop(assigned);
        let (fresh, _, fresh_generation) = collect_transcript_summaries_with_refresh(true);
        transcripts = fresh;
        generation = fresh_generation;
        assigned = assign_transcripts_with_state(&processes, &transcripts, state, true);
    }

    state.unmatched_index_generations.clear();
    state.unmatched_index_generations.extend(
        processes
            .iter()
            .filter(|process| !assigned.contains_key(&process.pid))
            .map(|process| (process.pid, generation)),
    );

    let mut sessions: Vec<AgentSession> = processes
        .into_iter()
        .map(|process| {
            let transcript = assigned.get(&process.pid).copied();
            session_from_live_process(process, transcript)
        })
        .collect();
    sessions.sort_by_key(|session| Reverse(session.started_at));
    sessions
}

fn session_from_live_process(
    process: LiveCodexProcess,
    transcript: Option<&CodexTranscriptSummary>,
) -> AgentSession {
    let session_id = transcript
        .map(|t| t.session_id.clone())
        .unwrap_or_else(|| format!("codex-{}", process.pid));

    let mut session = AgentSession::from_raw(crate::session::RawAgentSession {
        provider: crate::provider::AgentProvider::Codex,
        pid: process.pid,
        process_start_identity: Some(process.start_identity),
        session_id,
        cwd: process.cwd,
        started_at: process.started_at,
    });
    session.identity_provenance = if transcript.is_some() {
        SessionIdentityProvenance::Structured
    } else {
        SessionIdentityProvenance::ProcessOnly
    };
    session.tty = process.tty;
    session.cpu_percent = process.cpu_percent;
    session.mem_mb = process.mem_mb;
    session.command_args = process.command_args;

    if let Some(transcript) = transcript {
        session.jsonl_path = Some(transcript.path.clone());
        session.last_message_ts = transcript.mtime_ms;
        session.model_profile_source = "codex-transcript".into();
    }

    session
}

const START_TOLERANCE_MS: u64 = 10 * 60 * 1000;
const TRANSCRIPT_START_SKEW_MS: u64 = 2_000;

fn compatible_new_session(process: &LiveCodexProcess, transcript: &CodexTranscriptSummary) -> bool {
    process.cwd == transcript.cwd
        && transcript
            .started_at_ms
            .saturating_add(TRANSCRIPT_START_SKEW_MS)
            >= process.started_at
        && process.started_at.abs_diff(transcript.started_at_ms) <= START_TOLERANCE_MS
}

fn compatible_transition(
    process: &LiveCodexProcess,
    previous: &RetainedTranscript,
    candidate: &CodexTranscriptSummary,
) -> bool {
    candidate.cwd == process.cwd
        && candidate.session_id != previous.session_id
        && (candidate.mtime_ms > previous.transcript_mtime_ms
            || (!previous.resume_superseded
                && candidate.started_at_ms > previous.transcript_started_at_ms))
}

fn assign_transcripts<'a>(
    processes: &[LiveCodexProcess],
    transcripts: &'a [CodexTranscriptSummary],
    retained: &HashMap<u32, RetainedTranscript>,
) -> HashMap<u32, &'a CodexTranscriptSummary> {
    let mut assigned = HashMap::new();
    let mut used_paths = HashSet::new();
    let mut blocked_paths_by_pid = HashMap::new();

    for process in processes {
        let Some(previous) = retained.get(&process.pid) else {
            continue;
        };
        if process.start_identity != previous.process_start_identity {
            blocked_paths_by_pid.insert(process.pid, previous.path.clone());
            continue;
        }
        if extract_resume_uuid(&process.command_args).is_some_and(|resume_id| {
            previous.session_id != resume_id && !previous.resume_superseded
        }) {
            continue;
        }
        let Some(found) = transcripts.iter().find(|transcript| {
            transcript.session_id == previous.session_id
                && transcript.path == previous.path
                && transcript.cwd == process.cwd
                && !used_paths.contains(&transcript.path)
        }) else {
            continue;
        };
        used_paths.insert(found.path.clone());
        assigned.insert(process.pid, found);
    }

    let mut resume_candidates = Vec::new();
    let mut resume_claims: HashMap<PathBuf, usize> = HashMap::new();
    let mut blocked_heuristic_pids = HashSet::new();
    for process in processes
        .iter()
        .filter(|process| !assigned.contains_key(&process.pid))
    {
        let Some(resume_id) = extract_resume_uuid(&process.command_args) else {
            continue;
        };
        let candidates: Vec<_> = transcripts
            .iter()
            .filter(|transcript| {
                transcript.session_id == resume_id && transcript.cwd == process.cwd
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let [found] = candidates.as_slice() else {
            blocked_heuristic_pids.insert(process.pid);
            continue;
        };
        if used_paths.contains(&found.path) {
            blocked_heuristic_pids.insert(process.pid);
            continue;
        }
        *resume_claims.entry(found.path.clone()).or_default() += 1;
        resume_candidates.push((process.pid, *found));
    }
    for (pid, found) in resume_candidates {
        if resume_claims.get(&found.path) != Some(&1) {
            blocked_heuristic_pids.insert(pid);
            continue;
        }
        used_paths.insert(found.path.clone());
        assigned.insert(pid, found);
    }

    loop {
        let pending: Vec<&LiveCodexProcess> = processes
            .iter()
            .filter(|process| {
                !assigned.contains_key(&process.pid)
                    && !blocked_heuristic_pids.contains(&process.pid)
            })
            .collect();
        let mut mutual_best = Vec::new();

        for process in &pending {
            let mut candidates: Vec<&CodexTranscriptSummary> = transcripts
                .iter()
                .filter(|transcript| {
                    !used_paths.contains(&transcript.path)
                        && blocked_paths_by_pid.get(&process.pid) != Some(&transcript.path)
                        && compatible_new_session(process, transcript)
                })
                .collect();
            candidates.sort_by_key(|transcript| {
                (
                    process.started_at.abs_diff(transcript.started_at_ms),
                    transcript.path.as_os_str(),
                )
            });
            let Some(best) = candidates.first().copied() else {
                continue;
            };
            let best_distance = process.started_at.abs_diff(best.started_at_ms);
            if candidates.get(1).is_some_and(|next| {
                process.started_at.abs_diff(next.started_at_ms) == best_distance
            }) {
                continue;
            }

            let mut competing: Vec<(&LiveCodexProcess, u64)> = pending
                .iter()
                .filter(|candidate| compatible_new_session(candidate, best))
                .map(|candidate| {
                    (
                        *candidate,
                        candidate.started_at.abs_diff(best.started_at_ms),
                    )
                })
                .collect();
            competing.sort_by_key(|(candidate, distance)| (*distance, candidate.pid));
            let Some((best_process, best_process_distance)) = competing.first() else {
                continue;
            };
            if best_process.pid != process.pid
                || competing
                    .get(1)
                    .is_some_and(|(_, distance)| distance == best_process_distance)
            {
                continue;
            }
            mutual_best.push((best_distance, process.pid, best));
        }

        mutual_best.sort_by_key(|(distance, pid, transcript)| {
            (*distance, *pid, transcript.path.as_os_str())
        });
        let mut progress = false;
        for (_, pid, transcript) in mutual_best {
            if assigned.contains_key(&pid) || used_paths.contains(&transcript.path) {
                continue;
            }
            assigned.insert(pid, transcript);
            used_paths.insert(transcript.path.clone());
            progress = true;
        }
        if !progress {
            break;
        }
    }

    let mut activity_candidates = Vec::new();
    let mut activity_claims: HashMap<PathBuf, usize> = HashMap::new();
    for process in processes.iter().filter(|process| {
        !assigned.contains_key(&process.pid)
            && !blocked_heuristic_pids.contains(&process.pid)
            && process.command_args.trim().is_empty()
    }) {
        let candidates: Vec<_> = transcripts
            .iter()
            .filter(|transcript| {
                !used_paths.contains(&transcript.path)
                    && blocked_paths_by_pid.get(&process.pid) != Some(&transcript.path)
                    && transcript.cwd == process.cwd
                    && transcript
                        .started_at_ms
                        .saturating_add(TRANSCRIPT_START_SKEW_MS)
                        < process.started_at
                    && transcript.mtime_ms >= process.started_at
            })
            .collect();
        let [candidate] = candidates.as_slice() else {
            continue;
        };
        *activity_claims.entry(candidate.path.clone()).or_default() += 1;
        activity_candidates.push((process.pid, *candidate));
    }
    for (pid, candidate) in activity_candidates {
        if activity_claims.get(&candidate.path) != Some(&1) {
            continue;
        }
        used_paths.insert(candidate.path.clone());
        assigned.insert(pid, candidate);
    }

    assigned
}

fn assign_transcripts_with_state<'a>(
    processes: &[LiveCodexProcess],
    transcripts: &'a [CodexTranscriptSummary],
    state: &mut TranscriptAssignmentState,
    uncached_scan: bool,
) -> HashMap<u32, &'a CodexTranscriptSummary> {
    let mut assigned = assign_transcripts(processes, transcripts, &state.retained);
    let mut candidate_sets = Vec::new();
    let mut candidate_claims: HashMap<PathBuf, usize> = HashMap::new();
    let mut confirmed_transition_pids = HashSet::new();

    for process in processes {
        let Some(previous) = state.retained.get(&process.pid).cloned() else {
            continue;
        };
        if process.start_identity != previous.process_start_identity {
            state.transitions.remove(&process.pid);
            continue;
        }
        let Some(current) = assigned.get(&process.pid).copied() else {
            state.transitions.remove(&process.pid);
            continue;
        };
        if current.session_id != previous.session_id || current.path != previous.path {
            state.transitions.remove(&process.pid);
            continue;
        }
        if current.mtime_ms > previous.transcript_mtime_ms {
            state.transitions.remove(&process.pid);
            continue;
        }

        let used_by_others: HashSet<&PathBuf> = assigned
            .iter()
            .filter(|(pid, _)| **pid != process.pid)
            .map(|(_, transcript)| &transcript.path)
            .collect();
        let mut candidates: Vec<&CodexTranscriptSummary> = transcripts
            .iter()
            .filter(|candidate| {
                compatible_transition(process, &previous, candidate)
                    && !used_by_others.contains(&candidate.path)
            })
            .collect();
        if let Some(latest_mtime_ms) = candidates.iter().map(|candidate| candidate.mtime_ms).max() {
            candidates.retain(|candidate| candidate.mtime_ms == latest_mtime_ms);
        }
        candidates.sort_by_key(|candidate| candidate.path.as_os_str());
        if let [candidate] = candidates.as_slice() {
            *candidate_claims.entry(candidate.path.clone()).or_default() += 1;
        }
        candidate_sets.push((process.pid, candidates));
    }

    for (pid, candidates) in candidate_sets {
        let [candidate] = candidates.as_slice() else {
            state.transitions.remove(&pid);
            continue;
        };
        if candidate_claims.get(&candidate.path) != Some(&1) {
            state.transitions.remove(&pid);
            continue;
        }
        if !uncached_scan {
            if state.transitions.get(&pid).is_some_and(|pending| {
                pending.session_id != candidate.session_id || pending.path != candidate.path
            }) {
                state.transitions.remove(&pid);
            }
            continue;
        }

        let scans = match state.transitions.get(&pid) {
            Some(pending)
                if pending.session_id == candidate.session_id && pending.path == candidate.path =>
            {
                pending.consecutive_uncached_scans.saturating_add(1)
            }
            _ => 1,
        };
        if scans < 2 {
            state.transitions.insert(
                pid,
                PendingTranscriptTransition {
                    session_id: candidate.session_id.clone(),
                    path: candidate.path.clone(),
                    consecutive_uncached_scans: scans,
                },
            );
            continue;
        }

        assigned.insert(pid, candidate);
        confirmed_transition_pids.insert(pid);
        state.transitions.remove(&pid);
    }

    let live_pids: HashSet<u32> = processes.iter().map(|process| process.pid).collect();
    state.retained.retain(|pid, _| live_pids.contains(pid));
    state.transitions.retain(|pid, _| live_pids.contains(pid));
    for process in processes {
        let Some(transcript) = assigned.get(&process.pid) else {
            state.retained.remove(&process.pid);
            state.transitions.remove(&process.pid);
            continue;
        };
        let resume_superseded = confirmed_transition_pids.contains(&process.pid)
            || state.retained.get(&process.pid).is_some_and(|previous| {
                previous.process_start_identity == process.start_identity
                    && previous.session_id == transcript.session_id
                    && previous.path == transcript.path
                    && previous.resume_superseded
            });
        state.retained.insert(
            process.pid,
            RetainedTranscript {
                process_start_identity: process.start_identity,
                session_id: transcript.session_id.clone(),
                path: transcript.path.clone(),
                transcript_started_at_ms: transcript.started_at_ms,
                transcript_mtime_ms: transcript.mtime_ms,
                resume_superseded,
            },
        );
    }

    assigned
}

fn collect_rollout_jsonls(dir: &PathBuf, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_jsonls(&path, paths);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("rollout-"))
        {
            paths.push(path);
        }
    }
}

pub fn transcript_summary_from_codex_jsonl(path: PathBuf) -> Option<CodexTranscriptSummary> {
    let file = fs::File::open(&path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let Some(CodexEvent::SessionMeta(meta)) = parse_line(line.trim()) else {
            continue;
        };
        let started_at_ms = transcript_started_at_ms(meta.timestamp.as_deref())?;
        let mtime_ms = file_mtime_ms(&path).unwrap_or_default();
        return Some(CodexTranscriptSummary {
            session_id: meta.session_id,
            cwd: meta.cwd,
            path,
            started_at_ms,
            mtime_ms,
        });
    }
    None
}

fn transcript_started_at_ms(timestamp: Option<&str>) -> Option<u64> {
    let parsed = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Resolve JSONL paths for sessions. Must be called AFTER command_args are populated
/// (i.e., after fetch_ps_data), so we can use resume UUIDs for correct mapping.
pub fn resolve_jsonl_paths(sessions: &mut [AgentSession]) {
    for session in sessions.iter_mut() {
        if !session.process_backed {
            continue;
        }
        let slug = cwd_to_slug(&session.cwd);
        let project_dir = projects_dir().join(&slug);

        // Priority 1: Try the session's own ID in the expected project dir
        let own_path = project_dir.join(format!("{}.jsonl", session.session_id));
        if own_path.exists() {
            session.jsonl_path = Some(own_path);
            continue;
        }

        // Priority 2: Try the resume UUID from command args
        if let Some(resume_id) = extract_resume_uuid(&session.command_args) {
            let resume_path = project_dir.join(format!("{resume_id}.jsonl"));
            if resume_path.exists() {
                session.jsonl_path = Some(resume_path);
                continue;
            }
        }

        // Priority 3: Search ALL project directories for a JSONL matching the session ID.
        // This handles cwd encoding mismatches between codexctl and Codex
        // (e.g., symlink resolution, path normalization differences).
        if let Some(found) = search_all_projects_for_session(&session.session_id) {
            crate::logger::log(
                "DEBUG",
                &format!(
                    "session {}: slug mismatch — found JSONL via project scan: {}",
                    session.session_id,
                    found.display()
                ),
            );
            session.jsonl_path = Some(found);
            continue;
        }

        crate::logger::log(
            "DEBUG",
            &format!(
                "session {}: no JSONL found (slug={}, project_dir_exists={})",
                session.session_id,
                slug,
                project_dir.exists()
            ),
        );
    }
}

/// Search all directories under the Codex sessions root for a JSONL file matching the session ID.
/// This is a fallback when the cwd-based slug doesn't match the actual directory on disk.
fn search_all_projects_for_session(session_id: &str) -> Option<PathBuf> {
    let filename = format!("{session_id}.jsonl");
    let base = projects_dir();
    let entries = fs::read_dir(&base).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(&filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Extract the UUID from a resume argument in command args.
fn extract_resume_uuid(command_args: &str) -> Option<String> {
    let marker = if command_args.contains("--resume ") {
        "--resume "
    } else {
        "resume "
    };
    let start = command_args.find(marker)? + marker.len();
    let rest = &command_args[start..];
    // Take until whitespace — could be a UUID or a named session
    let token: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if token.is_empty() {
        return None;
    }
    // Strip surrounding quotes
    let token = token.trim_matches('"').trim_matches('\'');
    Some(token.to_string())
}

fn file_mtime_ms(path: &PathBuf) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64,
    )
}

/// Feature #29: Scan for subagent task .jsonl files.
/// Legacy sub-agent task files live in:
///   /tmp/codex-{uid}/{project_slug}/{sessionId}/tasks/
pub fn scan_subagents(sessions: &mut [AgentSession]) {
    let uid = unsafe { libc::getuid() };
    let tmp_base = PathBuf::from(format!("/tmp/codex-{uid}"));

    if !tmp_base.exists() {
        for session in sessions.iter_mut() {
            session.active_subagent_count = 0;
            session.active_subagent_jsonl_paths.clear();
        }
        return;
    }

    for session in sessions.iter_mut() {
        if !session.process_backed {
            session.active_subagent_count = 0;
            session.active_subagent_jsonl_paths.clear();
            continue;
        }
        let slug = cwd_to_slug(&session.cwd);
        let tasks_dir = tmp_base.join(&slug).join(&session.session_id).join("tasks");

        if !tasks_dir.exists() {
            session.active_subagent_count = 0;
            session.active_subagent_jsonl_paths.clear();
            continue;
        }

        let mut jsonls = Vec::new();
        collect_subagent_jsonls(&tasks_dir, &mut jsonls);
        jsonls.sort();
        session.active_subagent_count = jsonls.len();
        session.active_subagent_jsonl_paths = jsonls;
    }
}

fn collect_subagent_jsonls(dir: &PathBuf, jsonls: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_subagent_jsonls(&path, jsonls);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            jsonls.push(path);
        }
    }
}

/// Resolve git worktree identity for each session (for conflict detection).
/// Sessions in different worktrees of the same repo get different IDs.
/// Runs `git rev-parse --show-toplevel` once per unique cwd.
pub fn resolve_worktree_ids(sessions: &mut [AgentSession]) {
    // Cache results to avoid running git multiple times for the same cwd
    let mut cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for session in sessions.iter_mut() {
        if session.worktree_id.is_some() {
            continue;
        }
        let id = if let Some(cached) = cache.get(&session.cwd) {
            cached.clone()
        } else {
            let resolved = std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .current_dir(&session.cwd)
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout)
                            .ok()
                            .map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                })
                // Fall back to cwd if not a git repo
                .unwrap_or_else(|| session.cwd.clone());
            cache.insert(session.cwd.clone(), resolved.clone());
            resolved
        };
        session.worktree_id = Some(id);
    }
}

fn cwd_to_slug(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        return "-".to_string();
    }
    trimmed.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::provider::AgentProvider;

    static CODEX_HOME_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_tolerant_claude_inventory_fixtures() {
        let interactive = claude::parse_inventory(include_bytes!(
            "../../../tests/fixtures/claude-agents-interactive.json"
        ))
        .unwrap();
        assert_eq!(interactive.len(), 1);
        assert_eq!(interactive[0].provider, AgentProvider::Claude);
        assert_eq!(
            interactive[0].session_id.as_deref(),
            Some("interactive-session-uuid")
        );
        assert_eq!(interactive[0].attach_id, None);

        let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../tests/fixtures/claude-agents-background.json"
        ))
        .unwrap();
        let entry = claude::parse_inventory_entry(&fixture["agents"][0]).unwrap();
        assert_eq!(entry.provider, AgentProvider::Claude);
        assert_eq!(entry.session_id.as_deref(), Some("session-uuid"));
        assert_eq!(entry.attach_id.as_deref(), Some("agent-id"));
    }

    #[test]
    fn background_claude_attach_evidence_is_projected_explicitly() {
        let entry = claude::ClaudeInventoryEntry {
            provider: AgentProvider::Claude,
            session_id: Some("session-uuid".into()),
            attach_id: Some("agent-42".into()),
            cwd: PathBuf::from("/work/claude"),
            pid: None,
            started_at: Some(1),
            status: None,
        };

        let sessions = claude::sessions_from_inventory(&[entry], false, true, &[]);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-uuid");
        assert_eq!(sessions[0].native_attach_id.as_deref(), Some("agent-42"));
        assert_eq!(
            sessions[0].identity_provenance,
            crate::session::SessionIdentityProvenance::Structured
        );
    }

    #[test]
    fn claude_structured_inventory_identity_survives_process_encoding_collision() {
        let process = crate::process::ProcessSnapshotEntry::fixture(
            42,
            "pts/7",
            "claude",
            "/work/claude",
            9_001,
        );
        let colliding_id = process_session_id(&process);
        let entry = claude::ClaudeInventoryEntry {
            provider: AgentProvider::Claude,
            session_id: Some(colliding_id.clone()),
            attach_id: None,
            cwd: PathBuf::from("/work/claude"),
            pid: Some(42),
            started_at: Some(9_001),
            status: None,
        };

        let sessions = claude::sessions_from_inventory(&[entry], false, true, &[process]);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, colliding_id);
        assert!(sessions[0].process_backed);
        assert_eq!(
            sessions[0].identity_provenance,
            crate::session::SessionIdentityProvenance::Structured
        );
    }

    #[test]
    fn rejects_malformed_and_oversized_claude_inventory() {
        assert!(claude::parse_inventory(b"not json").is_err());
        let oversized = vec![b' '; claude::MAX_INVENTORY_BYTES + 1];
        assert!(claude::parse_inventory(&oversized).is_err());
    }

    #[test]
    fn failed_claude_refresh_retains_timestamped_stale_inventory() {
        let now = Instant::now();
        let existing = claude::ClaudeInventoryEntry {
            provider: AgentProvider::Claude,
            session_id: Some("stale-session".into()),
            attach_id: None,
            cwd: PathBuf::from("/work/stale"),
            pid: None,
            started_at: Some(1),
            status: None,
        };
        let mut cache = claude::ClaudeInventoryCache {
            refreshed_at: None,
            last_good: vec![existing.clone()],
            last_error: None,
        };

        let entries = claude::inventory_with_runner(&mut cache, now, |timeout, output_cap| {
            assert_eq!(timeout, Duration::from_secs(2));
            assert_eq!(output_cap, claude::MAX_INVENTORY_BYTES);
            Err(claude::InventoryError::Timeout)
        });

        assert_eq!(entries, vec![existing]);
        assert_eq!(cache.refreshed_at, Some(now));
        assert!(cache.last_error.as_deref().unwrap().contains("timed out"));
    }

    #[test]
    fn fresh_claude_inventory_cache_skips_command_runner() {
        let now = Instant::now();
        let mut cache = claude::ClaudeInventoryCache {
            refreshed_at: Some(now),
            last_good: Vec::new(),
            last_error: None,
        };
        let mut called = false;

        let entries = claude::inventory_with_runner(&mut cache, now, |_, _| {
            called = true;
            Ok(Vec::new())
        });

        assert!(entries.is_empty());
        assert!(!called);
    }

    #[test]
    fn provider_scan_uses_one_snapshot_and_merges_structured_and_process_evidence() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let now = Instant::now();
        let mut state = ProviderDiscoveryState::default();
        let mut process_scans = 0;
        let sessions = scan_agent_sessions_with_runners(
            &mut state,
            now,
            || {
                process_scans += 1;
                crate::process::ProcessSnapshot::from_entries(vec![
                    crate::process::ProcessSnapshotEntry::fixture(
                        11,
                        "pts/1",
                        "codex",
                        "/work/codex",
                        101,
                    ),
                    crate::process::ProcessSnapshotEntry::fixture(
                        12,
                        "pts/2",
                        "/usr/local/bin/claude",
                        "/work/claude-fallback",
                        102,
                    ),
                    crate::process::ProcessSnapshotEntry::fixture(
                        13,
                        "pts/3",
                        "agy",
                        "/work/agy",
                        103,
                    ),
                    crate::process::ProcessSnapshotEntry::fixture(
                        14,
                        "pts/4",
                        "claude-helper",
                        "/work/not-claude",
                        104,
                    ),
                    crate::process::ProcessSnapshotEntry::fixture(
                        15,
                        "pts/5",
                        "claude",
                        "/work/claude-process-only",
                        105,
                    ),
                ])
            },
            |_, _| {
                Ok(br#"[{"kind":"interactive","cwd":"/work/claude","startedAt":"1970-01-01T00:00:00.102Z","pid":12,"sessionId":"claude-native"}]"#.to_vec())
            },
        );

        assert_eq!(process_scans, 1);
        assert_eq!(sessions.len(), 4);
        let claude = sessions
            .iter()
            .find(|session| session.session_id == "claude-native")
            .unwrap();
        assert_eq!(claude.provider, AgentProvider::Claude);
        assert_eq!(claude.session_id, "claude-native");
        assert_eq!(claude.cwd, "/work/claude");
        assert_eq!(claude.pid, 12);
        assert_eq!(claude.process_start_identity, Some(102));
        assert_eq!(claude.tty, "pts/2");
        assert_eq!(
            claude.identity_provenance,
            crate::session::SessionIdentityProvenance::Structured
        );

        let codex_fallback = sessions
            .iter()
            .find(|session| session.provider == AgentProvider::Codex)
            .unwrap();
        assert_eq!(
            codex_fallback.identity_provenance,
            crate::session::SessionIdentityProvenance::ProcessOnly
        );

        let claude_fallback = sessions.iter().find(|session| session.pid == 15).unwrap();
        assert_eq!(claude_fallback.provider, AgentProvider::Claude);
        assert_eq!(
            claude_fallback.status,
            crate::session::SessionStatus::Unknown
        );
        assert!(claude_fallback.live_process_identity().is_some());
        assert_eq!(
            claude_fallback.identity_provenance,
            crate::session::SessionIdentityProvenance::ProcessOnly
        );

        let agy = sessions
            .iter()
            .find(|session| session.provider == AgentProvider::Antigravity)
            .unwrap();
        assert_eq!(agy.status, crate::session::SessionStatus::Unknown);
        assert!(agy.live_process_identity().is_some());
        assert_eq!(
            agy.identity_provenance,
            crate::session::SessionIdentityProvenance::ProcessOnly
        );
    }

    #[test]
    fn stale_claude_inventory_without_pid_survives_failed_refresh() {
        let now = Instant::now();
        let mut state = ProviderDiscoveryState {
            transcript_assignments: TranscriptAssignmentState::default(),
            claude_inventory: claude::ClaudeInventoryCache {
                refreshed_at: None,
                last_good: vec![claude::ClaudeInventoryEntry {
                    provider: AgentProvider::Claude,
                    session_id: Some("stale-session".into()),
                    attach_id: None,
                    cwd: PathBuf::from("/work/stale"),
                    pid: None,
                    started_at: Some(123_000),
                    status: None,
                }],
                last_error: None,
            },
        };

        let sessions = scan_agent_sessions_with_runners(
            &mut state,
            now,
            crate::process::ProcessSnapshot::default,
            |_, _| Err(claude::InventoryError::Oversized),
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].provider, AgentProvider::Claude);
        assert_eq!(sessions[0].session_id, "stale-session");
        assert!(!sessions[0].process_backed);
        assert_eq!(state.claude_inventory.refreshed_at, Some(now));
        assert!(
            state
                .claude_inventory
                .last_error
                .as_deref()
                .unwrap()
                .contains("one MiB")
        );
    }

    #[test]
    fn failed_process_snapshot_retains_stale_claude_pid_without_live_evidence() {
        let now = Instant::now();
        let mut state = provider_state_with_stale_claude_pid();

        let sessions = scan_agent_sessions_with_runners(
            &mut state,
            now,
            crate::process::ProcessSnapshot::default,
            |_, _| Err(claude::InventoryError::Timeout),
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "stale-pid-session");
        assert_eq!(sessions[0].pid, 42);
        assert!(!sessions[0].process_backed);
        assert!(sessions[0].live_process_identity().is_none());
    }

    #[test]
    fn successful_empty_process_snapshot_prunes_stale_claude_pid() {
        let now = Instant::now();
        let mut state = provider_state_with_stale_claude_pid();

        let sessions = scan_agent_sessions_with_runners(
            &mut state,
            now,
            || crate::process::ProcessSnapshot::from_entries(Vec::new()),
            |_, _| Err(claude::InventoryError::Timeout),
        );

        assert!(sessions.is_empty());
    }

    fn provider_state_with_stale_claude_pid() -> ProviderDiscoveryState {
        ProviderDiscoveryState {
            transcript_assignments: TranscriptAssignmentState::default(),
            claude_inventory: claude::ClaudeInventoryCache {
                refreshed_at: None,
                last_good: vec![claude::ClaudeInventoryEntry {
                    provider: AgentProvider::Claude,
                    session_id: Some("stale-pid-session".into()),
                    attach_id: None,
                    cwd: PathBuf::from("/work/stale"),
                    pid: Some(42),
                    started_at: Some(123_000),
                    status: None,
                }],
                last_error: None,
            },
        }
    }

    #[test]
    fn claude_inventory_does_not_bind_native_evidence_to_reused_pid() {
        let inventory = claude::ClaudeInventoryEntry {
            provider: AgentProvider::Claude,
            session_id: Some("old-native-session".into()),
            attach_id: None,
            cwd: PathBuf::from("/old/session"),
            pid: Some(42),
            started_at: Some(100_000),
            status: Some("working".into()),
        };
        let process = crate::process::ProcessSnapshotEntry {
            pid: 42,
            tty: "pts/42".into(),
            cpu_percent: 4.2,
            mem_mb: 64.0,
            command: "claude".into(),
            args: "claude".into(),
            cwd: PathBuf::from("/new/process"),
            started_at: 200_000,
            start_identity: 9001,
        };

        for (stale, started_at) in [
            (false, Some(100_000)),
            (true, Some(100_000)),
            (false, None),
            (true, None),
        ] {
            let mut inventory = inventory.clone();
            inventory.started_at = started_at;
            let sessions = claude::sessions_from_inventory(
                std::slice::from_ref(&inventory),
                stale,
                true,
                std::slice::from_ref(&process),
            );

            assert_eq!(sessions.len(), 2);
            let native = sessions
                .iter()
                .find(|session| session.session_id == "old-native-session")
                .unwrap();
            assert!(!native.process_backed);
            assert_eq!(native.process_start_identity, None);
            assert_eq!(native.cwd, "/old/session");
            let fallback = sessions
                .iter()
                .find(|session| session.process_backed)
                .unwrap();
            assert_eq!(fallback.cwd, "/new/process");
            assert_eq!(fallback.status, crate::session::SessionStatus::Unknown);
            assert_ne!(fallback.session_id, "old-native-session");
        }
    }

    fn transcript(id: &str, cwd: &str, start: u64, path: &str) -> CodexTranscriptSummary {
        CodexTranscriptSummary {
            session_id: id.into(),
            cwd: cwd.into(),
            path: PathBuf::from(path),
            started_at_ms: start,
            mtime_ms: start,
        }
    }

    fn process(pid: u32, cwd: &str, start: u64, args: &str) -> LiveCodexProcess {
        process_with_identity(pid, cwd, start, start, args)
    }

    fn process_with_identity(
        pid: u32,
        cwd: &str,
        start: u64,
        start_identity: u64,
        args: &str,
    ) -> LiveCodexProcess {
        LiveCodexProcess {
            pid,
            cwd: cwd.into(),
            started_at: start,
            start_identity,
            tty: format!("pts/{pid}"),
            cpu_percent: 0.0,
            mem_mb: 32.0,
            command_args: args.into(),
        }
    }

    fn retained(
        pid: u32,
        process_start: u64,
        session_id: &str,
        path: &str,
        transcript_start: u64,
    ) -> HashMap<u32, RetainedTranscript> {
        HashMap::from([(
            pid,
            RetainedTranscript {
                process_start_identity: process_start,
                session_id: session_id.into(),
                path: path.into(),
                transcript_started_at_ms: transcript_start,
                transcript_mtime_ms: transcript_start,
                resume_superseded: false,
            },
        )])
    }

    #[test]
    fn assigns_same_directory_processes_one_to_one_by_start_time() {
        let processes = vec![
            process(11, "/repo", 100_000, ""),
            process(12, "/repo", 200_000, ""),
        ];
        let transcripts = vec![
            transcript("first", "/repo", 101_000, "/rollout-first.jsonl"),
            transcript("second", "/repo", 201_000, "/rollout-second.jsonl"),
        ];

        let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

        assert_eq!(assigned[&11].session_id, "first");
        assert_eq!(assigned[&12].session_id, "second");
        assert_ne!(assigned[&11].path, assigned[&12].path);
    }

    #[test]
    fn explicit_resume_session_wins() {
        let processes = vec![process(11, "/repo", 100_000, "resume second")];
        let transcripts = vec![
            transcript("first", "/repo", 100_500, "/first.jsonl"),
            transcript("second", "/repo", 101_000, "/second.jsonl"),
        ];

        let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

        assert_eq!(assigned[&11].session_id, "second");
    }

    #[test]
    fn interactive_resume_attaches_unique_transcript_with_post_launch_activity() {
        let processes = vec![process(11, "/repo", 200_000, "")];
        let mut inactive = transcript("inactive", "/repo", 50_000, "/inactive.jsonl");
        inactive.mtime_ms = 150_000;
        let mut resumed = transcript("resumed", "/repo", 100_000, "/resumed.jsonl");
        resumed.mtime_ms = 250_000;
        let transcripts = [inactive, resumed];

        let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

        assert_eq!(assigned[&11].session_id, "resumed");
    }

    #[test]
    fn interactive_resume_with_multiple_active_transcripts_stays_unassigned() {
        let processes = vec![process(11, "/repo", 200_000, "")];
        let mut first = transcript("first", "/repo", 50_000, "/first.jsonl");
        first.mtime_ms = 250_000;
        let mut second = transcript("second", "/repo", 100_000, "/second.jsonl");
        second.mtime_ms = 260_000;
        let transcripts = [first, second];

        let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

        assert!(assigned.is_empty());
    }

    #[test]
    fn noninteractive_process_does_not_attach_old_activity() {
        let processes = vec![process(11, "/repo", 200_000, "exec task")];
        let mut old = transcript("old", "/repo", 100_000, "/old.jsonl");
        old.mtime_ms = 250_000;
        let transcripts = [old];

        let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

        assert!(assigned.is_empty());
    }

    #[test]
    fn explicit_resume_replaces_temporary_heuristic_assignment() {
        let processes = vec![process(11, "/repo", 100_000, "resume wanted")];
        let temporary = transcript("temporary", "/repo", 101_000, "/temporary.jsonl");
        let mut state = TranscriptAssignmentState::default();

        let first = assign_transcripts_with_state(
            &processes,
            std::slice::from_ref(&temporary),
            &mut state,
            true,
        );
        assert_eq!(first[&11].session_id, "temporary");

        let wanted = transcript("wanted", "/repo", 102_000, "/wanted.jsonl");
        let transcripts = [temporary, wanted];
        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);

        assert_eq!(assigned[&11].session_id, "wanted");
    }

    #[test]
    fn conflicting_explicit_resume_claims_are_order_independent() {
        let first = process(11, "/repo", 100_000, "resume shared");
        let second = process(12, "/repo", 104_000, "resume shared");
        let transcripts = vec![transcript("shared", "/repo", 100_500, "/shared.jsonl")];

        let forward = assign_transcripts(
            &[first.clone(), second.clone()],
            &transcripts,
            &HashMap::new(),
        );
        let reverse = assign_transcripts(&[second, first], &transcripts, &HashMap::new());

        assert!(forward.is_empty());
        assert!(reverse.is_empty());
    }

    #[test]
    fn resume_claimant_does_not_fallback_when_target_is_retained() {
        let owner = process(11, "/repo", 100_000, "");
        let claimant = process(12, "/repo", 200_000, "resume shared");
        let shared = transcript("shared", "/repo", 101_000, "/shared.jsonl");
        let unrelated = transcript("unrelated", "/repo", 201_000, "/unrelated.jsonl");
        let retained = retained(11, 100_000, "shared", "/shared.jsonl", 101_000);

        let transcripts = [shared, unrelated];
        let assigned = assign_transcripts(&[owner, claimant], &transcripts, &retained);

        assert_eq!(assigned[&11].session_id, "shared");
        assert!(!assigned.contains_key(&12));
    }

    #[test]
    fn retains_valid_attachment_when_mtime_order_changes() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let mut kept = transcript("kept", "/repo", 101_000, "/kept.jsonl");
        kept.mtime_ms = 150_000;
        let mut newer = transcript("newer", "/repo", 102_000, "/newer.jsonl");
        newer.mtime_ms = 200_000;
        let retained = retained(11, 100_000, "kept", "/kept.jsonl", 101_000);

        let transcripts = [kept, newer];
        let assigned = assign_transcripts(&processes, &transcripts, &retained);

        assert_eq!(assigned[&11].session_id, "kept");
    }

    #[test]
    fn reused_pid_does_not_inherit_retained_transcript() {
        let processes = vec![process(11, "/repo", 300_000, "")];
        let transcripts = vec![transcript("old", "/repo", 101_000, "/old.jsonl")];
        let retained = retained(11, 100_000, "old", "/old.jsonl", 101_000);

        let assigned = assign_transcripts(&processes, &transcripts, &retained);

        assert!(!assigned.contains_key(&11));
    }

    #[test]
    fn reused_pid_with_similar_display_start_does_not_inherit_transcript() {
        let processes = vec![process_with_identity(11, "/repo", 101_000, 999, "")];
        let transcripts = vec![transcript("old", "/repo", 101_000, "/old.jsonl")];
        let retained = retained(11, 100_000, "old", "/old.jsonl", 101_000);

        let assigned = assign_transcripts(&processes, &transcripts, &retained);

        assert!(!assigned.contains_key(&11));
    }

    #[test]
    fn parses_linux_proc_start_ticks_after_parenthesized_command() {
        let stat = "123 (codex worker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20";

        assert_eq!(crate::process::parse_proc_start_ticks(stat), Some(4242));
    }

    #[test]
    fn leaves_equally_close_new_process_unassigned() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let transcripts = vec![
            transcript("left", "/repo", 99_000, "/left.jsonl"),
            transcript("right", "/repo", 101_000, "/right.jsonl"),
        ];

        assert!(assign_transcripts(&processes, &transcripts, &HashMap::new()).is_empty());
    }

    #[test]
    fn clear_transition_requires_two_uncached_scans() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let old = transcript("old", "/repo", 101_000, "/old.jsonl");
        let new = transcript("new", "/repo", 250_000, "/new.jsonl");
        let transcripts = vec![old, new];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 101_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let first = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(first[&11].session_id, "old");
        assert_eq!(state.transitions[&11].consecutive_uncached_scans, 1);

        let second = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(second[&11].session_id, "new");
        assert_eq!(state.retained[&11].session_id, "new");
        assert!(!state.transitions.contains_key(&11));
    }

    #[test]
    fn retained_process_transitions_to_older_resumed_transcript_by_activity() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let mut resumed = transcript("resumed", "/repo", 150_000, "/resumed.jsonl");
        resumed.mtime_ms = 400_000;
        let mut completed = transcript("completed", "/repo", 300_000, "/completed.jsonl");
        completed.mtime_ms = 350_000;
        let transcripts = vec![resumed, completed];
        let mut retained_map = retained(11, 100_000, "completed", "/completed.jsonl", 300_000);
        retained_map.get_mut(&11).unwrap().transcript_mtime_ms = 350_000;
        let mut state = TranscriptAssignmentState {
            retained: retained_map,
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let first = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(first[&11].session_id, "completed");
        assert_eq!(state.transitions[&11].session_id, "resumed");

        let second = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(second[&11].session_id, "resumed");

        let third = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(third[&11].session_id, "resumed");
        assert!(state.transitions.is_empty());
    }

    #[test]
    fn clear_transition_selects_unique_most_recently_active_candidate() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let old = transcript("old", "/repo", 101_000, "/old.jsonl");
        let mut active = transcript("active", "/repo", 150_000, "/active.jsonl");
        active.mtime_ms = 400_000;
        let mut completed = transcript("completed", "/repo", 300_000, "/completed.jsonl");
        completed.mtime_ms = 350_000;
        let transcripts = vec![old, completed, active];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 101_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let first = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(first[&11].session_id, "old");
        assert_eq!(state.transitions[&11].session_id, "active");

        let second = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(second[&11].session_id, "active");
    }

    #[test]
    fn clear_transition_with_activity_tie_stays_on_retained_transcript() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let old = transcript("old", "/repo", 101_000, "/old.jsonl");
        let mut first = transcript("first", "/repo", 150_000, "/first.jsonl");
        first.mtime_ms = 400_000;
        let mut second = transcript("second", "/repo", 300_000, "/second.jsonl");
        second.mtime_ms = 400_000;
        let transcripts = vec![old, first, second];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 101_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);

        assert_eq!(assigned[&11].session_id, "old");
        assert!(state.transitions.is_empty());
    }

    #[test]
    fn clear_transition_can_start_hours_after_process_launch() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let old = transcript("old", "/repo", 101_000, "/old.jsonl");
        let new = transcript("new", "/repo", 3_700_000, "/new.jsonl");
        let transcripts = vec![old, new];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 101_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);

        assert_eq!(assigned[&11].session_id, "old");
        assert_eq!(state.transitions[&11].session_id, "new");

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(assigned[&11].session_id, "new");
        assert!(state.retained[&11].resume_superseded);

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(assigned[&11].session_id, "new");
    }

    #[test]
    fn resumed_process_can_transition_after_clear() {
        let processes = vec![process(11, "/repo", 100_000, "resume old")];
        let old = transcript("old", "/repo", 1_000, "/old.jsonl");
        let new = transcript("new", "/repo", 3_700_000, "/new.jsonl");
        let transcripts = vec![old, new];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 1_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);

        assert_eq!(assigned[&11].session_id, "old");
        assert_eq!(state.transitions[&11].session_id, "new");

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(assigned[&11].session_id, "new");
        assert!(state.retained[&11].resume_superseded);

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
        assert_eq!(assigned[&11].session_id, "new");
    }

    #[test]
    fn shared_clear_candidate_is_ambiguous_for_retained_processes() {
        let processes = vec![
            process(11, "/repo", 100_000, ""),
            process(12, "/repo", 200_000, ""),
        ];
        let old_one = transcript("old-one", "/repo", 101_000, "/old-one.jsonl");
        let old_two = transcript("old-two", "/repo", 201_000, "/old-two.jsonl");
        let new = transcript("new", "/repo", 3_700_000, "/new.jsonl");
        let transcripts = vec![old_one, old_two, new];
        let mut retained_map = retained(11, 100_000, "old-one", "/old-one.jsonl", 101_000);
        retained_map.extend(retained(12, 200_000, "old-two", "/old-two.jsonl", 201_000));
        let mut state = TranscriptAssignmentState {
            retained: retained_map,
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);

        assert_eq!(assigned[&11].session_id, "old-one");
        assert_eq!(assigned[&12].session_id, "old-two");
        assert!(state.transitions.is_empty());
    }

    #[test]
    fn cached_scan_does_not_advance_clear_transition() {
        let processes = vec![process(11, "/repo", 100_000, "")];
        let old = transcript("old", "/repo", 101_000, "/old.jsonl");
        let new = transcript("new", "/repo", 250_000, "/new.jsonl");
        let transcripts = vec![old, new];
        let mut state = TranscriptAssignmentState {
            retained: retained(11, 100_000, "old", "/old.jsonl", 101_000),
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let assigned = assign_transcripts_with_state(&processes, &transcripts, &mut state, false);

        assert_eq!(assigned[&11].session_id, "old");
        assert!(state.transitions.is_empty());
    }

    #[test]
    fn slug_basic_path() {
        assert_eq!(cwd_to_slug("/Users/foo/bar"), "-Users-foo-bar");
    }

    #[test]
    fn slug_trailing_slash() {
        // Must strip trailing slash — otherwise slug ends with "-" and won't match disk
        assert_eq!(
            cwd_to_slug("/Users/foo/bar/"),
            "-Users-foo-bar",
            "trailing slash must be stripped before slugifying"
        );
    }

    #[test]
    fn slug_multiple_trailing_slashes() {
        assert_eq!(cwd_to_slug("/Users/foo/bar///"), "-Users-foo-bar");
    }

    #[test]
    fn slug_with_hyphens_in_name() {
        assert_eq!(
            cwd_to_slug("/Users/dev/data-platform-answers"),
            "-Users-dev-data-platform-answers"
        );
    }

    #[test]
    fn slug_root() {
        assert_eq!(cwd_to_slug("/"), "-");
    }

    #[test]
    fn slug_single_component() {
        assert_eq!(cwd_to_slug("/tmp"), "-tmp");
    }

    #[test]
    fn transcript_history_without_live_processes_yields_no_sessions() {
        let transcript = CodexTranscriptSummary {
            session_id: "sess-history".into(),
            cwd: "/repo".into(),
            path: PathBuf::from("/tmp/rollout-history.jsonl"),
            started_at_ms: 10_000,
            mtime_ms: 10_000,
        };

        let sessions = sessions_from_live_processes(Vec::new(), &[transcript]);

        assert!(sessions.is_empty());
    }

    #[test]
    fn live_process_attaches_matching_recent_transcript() {
        let transcript = CodexTranscriptSummary {
            session_id: "sess-live".into(),
            cwd: "/repo".into(),
            path: PathBuf::from("/tmp/rollout-live.jsonl"),
            started_at_ms: 120_000,
            mtime_ms: 120_000,
        };
        let process = LiveCodexProcess {
            pid: 42,
            cwd: "/repo".into(),
            started_at: 100_000,
            start_identity: 100_000,
            tty: "pts/1".into(),
            cpu_percent: 3.5,
            mem_mb: 64.0,
            command_args: String::new(),
        };

        let sessions = sessions_from_live_processes(vec![process], &[transcript]);

        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].process_backed);
        assert_eq!(sessions[0].pid, 42);
        assert_eq!(sessions[0].session_id, "sess-live");
        assert_eq!(sessions[0].cwd, "/repo");
        assert_eq!(
            sessions[0].jsonl_path.as_deref(),
            Some(std::path::Path::new("/tmp/rollout-live.jsonl"))
        );
        assert_eq!(
            sessions[0].identity_provenance,
            SessionIdentityProvenance::Structured
        );
    }

    #[test]
    fn live_process_without_matching_transcript_still_appears_once() {
        let transcript = CodexTranscriptSummary {
            session_id: "sess-other".into(),
            cwd: "/other".into(),
            path: PathBuf::from("/tmp/rollout-other.jsonl"),
            started_at_ms: 120_000,
            mtime_ms: 120_000,
        };
        let process = LiveCodexProcess {
            pid: 99,
            cwd: "/repo".into(),
            started_at: 100_000,
            start_identity: 100_000,
            tty: "pts/2".into(),
            cpu_percent: 0.0,
            mem_mb: 32.0,
            command_args: String::new(),
        };

        let sessions = sessions_from_live_processes(vec![process], &[transcript]);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "codex-99");
        assert_eq!(sessions[0].jsonl_path, None);
        assert_eq!(
            sessions[0].identity_provenance,
            SessionIdentityProvenance::ProcessOnly
        );
    }

    #[test]
    fn transcript_summary_scan_reuses_fresh_index() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        let first = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07")
            .join("rollout-first.jsonl");
        write_transcript(&first, "sess-first", "/repo");

        unsafe {
            std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        }
        let summaries = collect_transcript_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "sess-first");

        let second = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07")
            .join("rollout-second.jsonl");
        write_transcript(&second, "sess-second", "/repo");

        let summaries = collect_transcript_summaries();
        unsafe {
            std::env::remove_var("CODEXCTL_CODEX_HOME");
        }

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "sess-first");
    }

    #[test]
    fn unmatched_process_forces_uncached_summary_refresh() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07");
        write_transcript(&sessions.join("rollout-other.jsonl"), "other", "/other");

        unsafe {
            std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        }
        assert_eq!(collect_transcript_summaries().len(), 1);

        write_transcript(&sessions.join("rollout-live.jsonl"), "live", "/repo");
        let started_at = transcript_started_at_ms(Some("2026-07-07T00:00:00Z")).unwrap();
        let mut state = TranscriptAssignmentState::default();
        let discovered = sessions_from_discovered_processes(
            vec![process(42, "/repo", started_at, "")],
            &mut state,
        );
        unsafe {
            std::env::remove_var("CODEXCTL_CODEX_HOME");
        }

        assert_eq!(discovered[0].session_id, "live");
        assert_eq!(state.retained[&42].session_id, "live");
    }

    #[test]
    fn persistently_unmatched_process_does_not_refresh_index_twice() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07");
        write_transcript(&sessions.join("rollout-other.jsonl"), "other", "/other");

        unsafe {
            std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        }
        let started_at = transcript_started_at_ms(Some("2026-07-07T00:00:00Z")).unwrap();
        let mut state = TranscriptAssignmentState::default();
        let live_process = process(42, "/repo", started_at, "");

        let first = sessions_from_discovered_processes(vec![live_process.clone()], &mut state);
        assert_eq!(first[0].session_id, "codex-42");
        let first_refresh = transcript_index_cache()
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .refreshed_at;

        let second = sessions_from_discovered_processes(vec![live_process], &mut state);
        let second_refresh = transcript_index_cache()
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .refreshed_at;
        unsafe {
            std::env::remove_var("CODEXCTL_CODEX_HOME");
        }

        assert_eq!(second[0].session_id, "codex-42");
        assert_eq!(second_refresh, first_refresh);
    }

    #[test]
    fn clear_transition_requires_two_outer_scans() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07");
        let old_path = sessions.join("rollout-old.jsonl");
        let new_path = sessions.join("rollout-new.jsonl");
        write_transcript_at(&old_path, "old", "/repo", "2026-07-07T00:00:00Z");
        write_transcript_at(&new_path, "new", "/repo", "2026-07-07T02:00:00Z");

        unsafe {
            std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        }
        let process_start = transcript_started_at_ms(Some("2026-07-07T00:00:00Z")).unwrap();
        let mut retained = retained(
            42,
            process_start,
            "old",
            old_path.to_str().unwrap(),
            process_start,
        );
        retained.get_mut(&42).unwrap().transcript_mtime_ms = file_mtime_ms(&old_path).unwrap();
        let mut state = TranscriptAssignmentState {
            retained,
            transitions: HashMap::new(),
            unmatched_index_generations: HashMap::new(),
        };

        let first = sessions_from_discovered_processes(
            vec![process(42, "/repo", process_start, "")],
            &mut state,
        );
        assert_eq!(first[0].session_id, "old");
        assert_eq!(state.transitions[&42].consecutive_uncached_scans, 1);

        let second = sessions_from_discovered_processes(
            vec![process(42, "/repo", process_start, "")],
            &mut state,
        );
        unsafe {
            std::env::remove_var("CODEXCTL_CODEX_HOME");
        }
        assert_eq!(second[0].session_id, "new");
    }

    #[test]
    fn fresh_state_seeds_transition_on_first_outer_scan() {
        let _guard = CODEX_HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07");
        let old_path = sessions.join("rollout-old.jsonl");
        let new_path = sessions.join("rollout-new.jsonl");
        write_transcript_at(&old_path, "old", "/repo", "2026-07-07T00:00:00Z");
        write_transcript_at(&new_path, "new", "/repo", "2026-07-07T02:00:00Z");

        unsafe {
            std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        }
        let process_start = transcript_started_at_ms(Some("2026-07-07T00:00:00Z")).unwrap();
        let live_process = process(42, "/repo", process_start, "");
        let mut state = TranscriptAssignmentState::default();

        let first = sessions_from_discovered_processes(vec![live_process.clone()], &mut state);
        assert_eq!(first[0].session_id, "old");
        assert_eq!(state.transitions[&42].consecutive_uncached_scans, 1);

        let second = sessions_from_discovered_processes(vec![live_process], &mut state);
        unsafe {
            std::env::remove_var("CODEXCTL_CODEX_HOME");
        }
        assert_eq!(second[0].session_id, "new");
    }

    fn write_transcript(path: &std::path::Path, session_id: &str, cwd: &str) {
        write_transcript_at(path, session_id, cwd, "2026-07-07T00:00:00Z");
    }

    fn write_transcript_at(path: &std::path::Path, session_id: &str, cwd: &str, timestamp: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"{session_id}","timestamp":"{timestamp}","cwd":"{cwd}","model_provider":"openai"}}}}"#
            ),
        )
        .unwrap();
    }
}
