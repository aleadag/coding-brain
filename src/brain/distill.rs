use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::decisions::{DecisionRecord, project_slug, read_distillation_decisions};
use super::pref_store::{parse_preferences_json, preferences_to_json};
use super::preferences::{DistilledPreferences, distill_preferences};

const SCHEMA_VERSION: u32 = 1;
const DISTILL_INTERVAL: usize = 10;
const MIN_PROJECT_DECISIONS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistillWatermark {
    pub schema_version: u32,
    pub through_decision_id: Option<String>,
    pub generation_id: Option<String>,
}

impl Default for DistillWatermark {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            through_decision_id: None,
            generation_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistillOutcome {
    Updated {
        processed: usize,
        generation_id: String,
    },
    NotDue {
        pending: usize,
    },
    AlreadyRunning,
}

#[derive(Debug)]
pub enum DistillError {
    Io(io::Error),
    Json(serde_json::Error),
    UnsupportedSchema(u32),
    MissingWatermarkDecision(String),
    NoStableDecisionId,
    AlreadyRunning,
    Injected(&'static str),
}

impl fmt::Display for DistillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "distillation I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "distillation JSON failed: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported distillation schema {version}")
            }
            Self::MissingWatermarkDecision(id) => {
                write!(formatter, "watermark decision {id} is absent")
            }
            Self::NoStableDecisionId => formatter.write_str("distillation batch has no stable ID"),
            Self::AlreadyRunning => formatter.write_str("distillation is already running"),
            Self::Injected(stage) => write!(formatter, "injected failure at {stage}"),
        }
    }
}

impl std::error::Error for DistillError {}

impl From<io::Error> for DistillError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DistillError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailStage {
    AfterGenerationFile(usize),
    BeforePointerSwap,
}

#[derive(Debug, Serialize, Deserialize)]
struct GenerationManifest {
    schema_version: u32,
    generation_id: String,
    projects: Vec<String>,
}

pub fn run_once(paths: &CodingBrainPaths) -> Result<DistillOutcome, DistillError> {
    let Some(lock) = try_acquire_lock(paths)? else {
        return Ok(DistillOutcome::AlreadyRunning);
    };
    let (cursor_decisions, learning_decisions) = read_distillation_decisions();
    finish_locked(
        &lock,
        run_locked(paths, &cursor_decisions, &learning_decisions, None),
    )
}

pub fn current_paths() -> io::Result<CodingBrainPaths> {
    CodingBrainPaths::resolve(&PathEnvironment::current())
        .map_err(|error| io::Error::other(format!("{error:?}")))
}

#[cfg(test)]
fn run_once_with_decisions(
    paths: &CodingBrainPaths,
    decisions: &[DecisionRecord],
    fail_stage: Option<FailStage>,
) -> Result<DistillOutcome, DistillError> {
    let Some(lock) = try_acquire_lock(paths)? else {
        return Ok(DistillOutcome::AlreadyRunning);
    };
    finish_locked(&lock, run_locked(paths, decisions, decisions, fail_stage))
}

#[cfg(test)]
fn run_once_with_inputs(
    paths: &CodingBrainPaths,
    cursor_decisions: &[DecisionRecord],
    learning_decisions: &[DecisionRecord],
) -> Result<DistillOutcome, DistillError> {
    let Some(lock) = try_acquire_lock(paths)? else {
        return Ok(DistillOutcome::AlreadyRunning);
    };
    finish_locked(
        &lock,
        run_locked(paths, cursor_decisions, learning_decisions, None),
    )
}

fn finish_locked(
    lock: &File,
    result: Result<DistillOutcome, DistillError>,
) -> Result<DistillOutcome, DistillError> {
    let unlock = FileExt::unlock(lock).map_err(DistillError::Io);
    match result {
        Err(error) => Err(error),
        Ok(outcome) => unlock.map(|()| outcome),
    }
}

fn try_acquire_lock(paths: &CodingBrainPaths) -> Result<Option<File>, DistillError> {
    let root = brain_root(paths);
    create_private_dir(&root)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("distill.lock"))?;
    set_file_mode(&lock)?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn run_locked(
    paths: &CodingBrainPaths,
    cursor_decisions: &[DecisionRecord],
    learning_decisions: &[DecisionRecord],
    fail_stage: Option<FailStage>,
) -> Result<DistillOutcome, DistillError> {
    let root = brain_root(paths);
    let previous = read_watermark(paths)?;
    let start = match previous.through_decision_id.as_deref() {
        Some(id) => cursor_decisions
            .iter()
            .position(|decision| decision.decision_id.as_deref() == Some(id))
            .map(|position| position + 1)
            .ok_or_else(|| DistillError::MissingWatermarkDecision(id.into()))?,
        None => 0,
    };
    let pending = cursor_decisions.len().saturating_sub(start);
    if pending < DISTILL_INTERVAL {
        return Ok(DistillOutcome::NotDue { pending });
    }
    if cursor_decisions.is_empty() {
        return Ok(DistillOutcome::NotDue { pending: 0 });
    }
    let candidates = &cursor_decisions[start..];
    let through_decision_id = candidates
        .iter()
        .rev()
        .find_map(|decision| decision.decision_id.clone())
        .ok_or(DistillError::NoStableDecisionId)?;

    let generation_decisions = learning_decisions;
    let global = distill_preferences(generation_decisions);
    let mut projects = HashMap::<String, Vec<DecisionRecord>>::new();
    for decision in generation_decisions {
        projects
            .entry(decision.project.to_lowercase())
            .or_default()
            .push(decision.clone());
    }
    let generation_id = generation_id();
    write_generation(paths, &generation_id, &global, &projects, fail_stage)?;
    #[cfg(debug_assertions)]
    pause_for_process_test("before-pointer")?;
    if fail_stage == Some(FailStage::BeforePointerSwap) {
        return Err(DistillError::Injected("before-pointer-swap"));
    }
    let watermark = DistillWatermark {
        schema_version: SCHEMA_VERSION,
        through_decision_id: Some(through_decision_id),
        generation_id: Some(generation_id.clone()),
    };
    atomic_write_json(&watermark_path(paths), &watermark)?;
    sync_directory(&root)?;
    cleanup_generations(paths, &generation_id, previous.generation_id.as_deref());
    Ok(DistillOutcome::Updated {
        processed: pending,
        generation_id,
    })
}

pub fn spawn_one_shot_if_due(paths: &CodingBrainPaths) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    if executable.file_stem().and_then(|name| name.to_str()) != Some("codexctl") {
        return Ok(());
    }
    if !record_trigger(paths)? {
        return Ok(());
    }
    let child = Command::new(executable)
        .arg("--distill-once")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    reap_child(child);
    Ok(())
}

fn reap_child(mut child: std::process::Child) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = child.wait();
    })
}

fn record_trigger(paths: &CodingBrainPaths) -> io::Result<bool> {
    let root = brain_root(paths);
    create_private_dir(&root)?;
    let mut trigger = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .truncate(false)
        .open(root.join("distill-trigger"))?;
    set_file_mode(&trigger)?;
    match trigger.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
        Err(error) => return Err(error),
    }
    let due = trigger.metadata()?.len().saturating_add(1) >= DISTILL_INTERVAL as u64;
    if due {
        trigger.set_len(0)?;
    } else {
        trigger.write_all(b"x")?;
        trigger.flush()?;
    }
    FileExt::unlock(&trigger)?;
    Ok(due)
}

pub(crate) fn forget_preferences_with(
    paths: &CodingBrainPaths,
    erase_source: impl FnOnce() -> io::Result<()>,
) -> Result<(), DistillError> {
    let Some(lock) = try_acquire_lock(paths)? else {
        return Err(DistillError::AlreadyRunning);
    };
    let root = brain_root(paths);
    let result = (|| {
        erase_source()?;
        remove_file_if_present(&watermark_path(paths))?;
        remove_file_if_present(&root.join("distill-trigger"))?;
        let generations = generations_root(paths);
        match fs::remove_dir_all(&generations) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        sync_directory(&root)?;
        Ok(DistillOutcome::NotDue { pending: 0 })
    })();
    finish_locked(&lock, result).map(|_| ())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_watermark(paths: &CodingBrainPaths) -> Result<DistillWatermark, DistillError> {
    let bytes = match fs::read(watermark_path(paths)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DistillWatermark::default());
        }
        Err(error) => return Err(error.into()),
    };
    let watermark: DistillWatermark = serde_json::from_slice(&bytes)?;
    if watermark.schema_version != SCHEMA_VERSION {
        return Err(DistillError::UnsupportedSchema(watermark.schema_version));
    }
    Ok(watermark)
}

pub(crate) fn load_global(paths: &CodingBrainPaths) -> Option<DistilledPreferences> {
    load_generation(paths, None)
}

pub(crate) fn load_project(
    paths: &CodingBrainPaths,
    project: &str,
) -> Option<DistilledPreferences> {
    load_generation(paths, Some(&project_slug(project)))
}

fn load_generation(
    paths: &CodingBrainPaths,
    project: Option<&str>,
) -> Option<DistilledPreferences> {
    let watermark = read_watermark(paths).ok()?;
    let generation_id = watermark.generation_id?;
    let root = generations_root(paths).join(&generation_id);
    let manifest: GenerationManifest =
        serde_json::from_slice(&fs::read(root.join("generation.json")).ok()?).ok()?;
    if manifest.schema_version != SCHEMA_VERSION || manifest.generation_id != generation_id {
        return None;
    }
    let path = match project {
        Some(project) if manifest.projects.iter().any(|item| item == project) => {
            root.join("projects").join(format!("{project}.json"))
        }
        Some(_) => return None,
        None => root.join("global.json"),
    };
    let json: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    parse_preferences_json(&json)
}

fn write_generation(
    paths: &CodingBrainPaths,
    generation_id: &str,
    global: &DistilledPreferences,
    projects: &HashMap<String, Vec<DecisionRecord>>,
    fail_stage: Option<FailStage>,
) -> Result<(), DistillError> {
    let generations = generations_root(paths);
    create_private_dir(&generations)?;
    let root = generations.join(generation_id);
    create_private_dir(&root)?;
    write_json_file(&root.join("global.json"), &preferences_to_json(global))?;
    let mut files_written = 1;
    fail_after_generation_file(fail_stage, files_written)?;
    let projects_root = root.join("projects");
    create_private_dir(&projects_root)?;
    let mut published_projects = Vec::new();
    for (project, decisions) in projects {
        if decisions.len() < MIN_PROJECT_DECISIONS {
            continue;
        }
        let slug = project_slug(project);
        let preferences = distill_preferences(decisions);
        write_json_file(
            &projects_root.join(format!("{slug}.json")),
            &preferences_to_json(&preferences),
        )?;
        files_written += 1;
        fail_after_generation_file(fail_stage, files_written)?;
        published_projects.push(slug);
    }
    published_projects.sort();
    write_json_file(
        &root.join("generation.json"),
        &GenerationManifest {
            schema_version: SCHEMA_VERSION,
            generation_id: generation_id.into(),
            projects: published_projects,
        },
    )?;
    files_written += 1;
    fail_after_generation_file(fail_stage, files_written)?;
    sync_directory(&projects_root)?;
    sync_directory(&root)?;
    sync_directory(&generations_root(paths))?;
    Ok(())
}

fn fail_after_generation_file(
    fail_stage: Option<FailStage>,
    files_written: usize,
) -> Result<(), DistillError> {
    if fail_stage == Some(FailStage::AfterGenerationFile(files_written)) {
        return Err(DistillError::Injected("after-generation-file"));
    }
    #[cfg(debug_assertions)]
    pause_for_process_test(&format!("file-{files_written}"))?;
    Ok(())
}

#[cfg(debug_assertions)]
fn pause_for_process_test(stage: &str) -> Result<(), DistillError> {
    if std::env::var("CODING_BRAIN_DISTILL_TEST_PAUSE").as_deref() != Ok(stage) {
        return Ok(());
    }
    let marker = std::env::var_os("CODING_BRAIN_DISTILL_TEST_MARKER")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("missing distill test marker"))?;
    fs::write(marker, stage)?;
    loop {
        std::thread::park();
    }
}

fn cleanup_generations(paths: &CodingBrainPaths, current: &str, previous: Option<&str>) {
    let Ok(entries) = fs::read_dir(generations_root(paths)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == current || previous.is_some_and(|previous| name == previous) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub(crate) fn brain_root(paths: &CodingBrainPaths) -> PathBuf {
    paths.state_root().join("brain")
}

fn generations_root(paths: &CodingBrainPaths) -> PathBuf {
    brain_root(paths).join("preferences-generations")
}

fn watermark_path(paths: &CodingBrainPaths) -> PathBuf {
    brain_root(paths).join("distill-watermark.json")
}

fn generation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("gen_{nanos}_{}", std::process::id())
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<(), DistillError> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    set_file_mode(&file)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), DistillError> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "watermark has no parent"))?;
    create_private_dir(parent)?;
    let temp = parent.join(format!(".distill-watermark-{}", generation_id()));
    write_json_file(&temp, value)?;
    fs::rename(&temp, path)?;
    sync_directory(parent)?;
    Ok(())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_directory_mode(path)
}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> io::Result<()> {
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

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use fs2::FileExt;

    use super::*;
    use crate::brain::decisions::{DecisionRecord, DecisionType};
    use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};

    fn paths(temp: &tempfile::TempDir) -> CodingBrainPaths {
        CodingBrainPaths::resolve(&PathEnvironment::new(
            Some(temp.path().join("config")),
            Some(temp.path().join("state")),
            Some(temp.path().join("home")),
        ))
        .unwrap()
    }

    fn decisions(count: usize) -> Vec<DecisionRecord> {
        (1..=count)
            .map(|index| DecisionRecord {
                provider: coding_brain_core::provider::AgentProvider::Codex,
                timestamp: index.to_string(),
                pid: 1,
                project: if index % 2 == 0 { "alpha" } else { "beta" }.into(),
                tool: Some("Bash".into()),
                command: Some("cargo test".into()),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "fixture".into(),
                user_action: "accept".into(),
                context: None,
                outcome: None,
                decision_type: DecisionType::Session,
                suggested_at: None,
                resolved_at: None,
                override_reason: None,
                decision_id: Some(format!("dec_{index}")),
                brain_decision_ms: None,
                cache_hit: None,
                canonical: None,
            })
            .collect()
    }

    #[test]
    fn successful_run_advances_to_last_processed_decision() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);

        let outcome = run_once_with_decisions(&paths, &decisions(25), None).unwrap();

        assert!(matches!(
            outcome,
            DistillOutcome::Updated { processed: 25, .. }
        ));
        let watermark = read_watermark(&paths).unwrap();
        assert_eq!(watermark.through_decision_id.as_deref(), Some("dec_25"));
        assert_eq!(load_global(&paths).unwrap().total_decisions, 25);
    }

    #[test]
    fn fewer_than_ten_pending_decisions_is_not_due() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);

        assert_eq!(
            run_once_with_decisions(&paths, &decisions(9), None).unwrap(),
            DistillOutcome::NotDue { pending: 9 }
        );
        assert!(read_watermark(&paths).unwrap().generation_id.is_none());
    }

    #[test]
    fn second_worker_exits_when_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let lock_path = brain_root(&paths).join("distill.lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let held = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        FileExt::lock_exclusive(&held).unwrap();

        assert_eq!(
            run_once_with_decisions(&paths, &decisions(25), None).unwrap(),
            DistillOutcome::AlreadyRunning
        );
    }

    #[test]
    fn crash_before_pointer_swap_keeps_previous_generation_visible() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        let previous = read_watermark(&paths).unwrap();

        let error =
            run_once_with_decisions(&paths, &decisions(20), Some(FailStage::BeforePointerSwap))
                .unwrap_err();

        assert!(matches!(error, DistillError::Injected(_)));
        assert_eq!(read_watermark(&paths).unwrap(), previous);
        assert_eq!(load_global(&paths).unwrap().total_decisions, 10);
    }

    #[test]
    fn crash_during_generation_keeps_previous_visible_and_retries() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        let previous = read_watermark(&paths).unwrap();

        let error = run_once_with_decisions(
            &paths,
            &decisions(20),
            Some(FailStage::AfterGenerationFile(2)),
        )
        .unwrap_err();

        assert!(matches!(error, DistillError::Injected(_)));
        assert_eq!(read_watermark(&paths).unwrap(), previous);
        assert_eq!(load_global(&paths).unwrap().total_decisions, 10);
        assert!(matches!(
            run_once_with_decisions(&paths, &decisions(20), None).unwrap(),
            DistillOutcome::Updated { processed: 10, .. }
        ));
        assert_eq!(load_global(&paths).unwrap().total_decisions, 20);
    }

    #[test]
    fn every_generation_file_failure_keeps_previous_visible() {
        for files_written in 1..=4 {
            let temp = tempfile::tempdir().unwrap();
            let paths = paths(&temp);
            run_once_with_decisions(&paths, &decisions(10), None).unwrap();
            let previous = read_watermark(&paths).unwrap();

            let error = run_once_with_decisions(
                &paths,
                &decisions(20),
                Some(FailStage::AfterGenerationFile(files_written)),
            )
            .unwrap_err();

            assert!(matches!(error, DistillError::Injected(_)));
            assert_eq!(read_watermark(&paths).unwrap(), previous);
            assert_eq!(load_global(&paths).unwrap().total_decisions, 10);
        }
    }

    #[test]
    fn reader_rejects_mismatched_generation_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        let generation = read_watermark(&paths).unwrap().generation_id.unwrap();
        let manifest = generations_root(&paths)
            .join(generation)
            .join("generation.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        value["generation_id"] = "different-generation".into();
        std::fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();

        assert!(load_global(&paths).is_none());
    }

    #[test]
    fn reader_rejects_corrupt_generation_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        let generation = read_watermark(&paths).unwrap().generation_id.unwrap();
        let global = generations_root(&paths)
            .join(generation)
            .join("global.json");
        std::fs::write(global, b"not json").unwrap();

        assert!(load_global(&paths).is_none());
    }

    #[test]
    fn compacted_learning_record_does_not_invalidate_raw_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let raw = decisions(20);
        run_once_with_decisions(&paths, &raw[..10], None).unwrap();
        let learning = raw[10..].to_vec();

        let outcome = run_once_with_inputs(&paths, &raw, &learning).unwrap();

        assert!(matches!(
            outcome,
            DistillOutcome::Updated { processed: 10, .. }
        ));
        assert_eq!(
            read_watermark(&paths)
                .unwrap()
                .through_decision_id
                .as_deref(),
            Some("dec_20")
        );
        assert_eq!(load_global(&paths).unwrap().total_decisions, 10);
    }

    #[test]
    fn trigger_is_due_only_on_every_tenth_append() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);

        for _ in 0..9 {
            assert!(!record_trigger(&paths).unwrap());
        }
        assert!(record_trigger(&paths).unwrap());
        assert!(!record_trigger(&paths).unwrap());
    }

    #[test]
    fn forgetting_preferences_removes_published_generation_and_watermark() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        assert!(load_global(&paths).is_some());

        forget_preferences_with(&paths, || Ok(())).unwrap();

        assert!(load_global(&paths).is_none());
        assert_eq!(read_watermark(&paths).unwrap(), DistillWatermark::default());
        assert!(!generations_root(&paths).exists());
    }

    #[test]
    fn forget_holds_both_locks_until_source_and_preferences_are_erased() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let source_root = temp.path().join("legacy-brain");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("decisions.jsonl"), b"old decisions\n").unwrap();
        run_once_with_decisions(&paths, &decisions(10), None).unwrap();
        let source_erased = Arc::new(Barrier::new(2));
        let release_forget = Arc::new(Barrier::new(2));
        let forget_paths = paths.clone();
        let forget_source = source_root.clone();
        let erased_in_thread = Arc::clone(&source_erased);
        let release_in_thread = Arc::clone(&release_forget);
        let forget = std::thread::spawn(move || {
            super::super::decisions::forget_at_with(&forget_paths, &forget_source, || {
                erased_in_thread.wait();
                release_in_thread.wait();
            })
        });
        source_erased.wait();

        assert_eq!(
            run_once_with_decisions(&paths, &decisions(20), None).unwrap(),
            DistillOutcome::AlreadyRunning
        );
        release_forget.wait();
        forget.join().unwrap().unwrap();

        assert!(!source_root.join("decisions.jsonl").exists());
        assert!(load_global(&paths).is_none());
        assert_eq!(read_watermark(&paths).unwrap(), DistillWatermark::default());
    }

    #[test]
    fn background_reaper_waits_for_child_exit() {
        let child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();

        reap_child(child).join().unwrap();
    }
}
