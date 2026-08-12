#![cfg(all(unix, feature = "fault-injection"))]

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use coding_brain::brain::storage::{
    BrainDb, MigrationCoordinator, MigrationStatus, OpenRole, StorageDeadline, StoragePaths,
};
use coding_brain_core::brain_activity::{
    ActivityEvent, ActivityKind, ActivityState, SessionTargetProvenance,
};
use coding_brain_core::lifecycle::{
    ApplyOutcome, LifecycleEvent, LifecycleEventKind, LifecycleEventName, LifecycleIdentity,
};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::{ProjectId, ProjectIdentity};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use rusqlite::{Connection, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

const PROVIDERS: [AgentProvider; 3] = [
    AgentProvider::Codex,
    AgentProvider::Claude,
    AgentProvider::Antigravity,
];

static LIVE_FAULT_MATRIX_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MatrixFault {
    AdmissionWrite,
    InferenceExit,
    CommitBeforeCall,
    CommitAfterReturn,
    StdoutWrite,
    DeliveryWrite,
    Checkpoint,
    MigrationPublish,
    CacheCommitBeforeCall,
    CacheCommitAfterReturn,
}

const FAULTS: [MatrixFault; 8] = [
    MatrixFault::AdmissionWrite,
    MatrixFault::InferenceExit,
    MatrixFault::CommitBeforeCall,
    MatrixFault::CommitAfterReturn,
    MatrixFault::StdoutWrite,
    MatrixFault::DeliveryWrite,
    MatrixFault::Checkpoint,
    MatrixFault::MigrationPublish,
];

impl MatrixFault {
    const fn as_cli_value(self) -> &'static str {
        match self {
            Self::AdmissionWrite => "admission-write",
            Self::InferenceExit => "inference-exit",
            Self::CommitBeforeCall => "commit-before-call",
            Self::CommitAfterReturn => "commit-after-return",
            Self::StdoutWrite => "stdout-write",
            Self::DeliveryWrite => "delivery-write",
            Self::Checkpoint => "checkpoint",
            Self::MigrationPublish => "migration-publish",
            Self::CacheCommitBeforeCall => "cache-commit-before-call",
            Self::CacheCommitAfterReturn => "cache-commit-after-return",
        }
    }

    const fn marker_position(self) -> &'static str {
        match self {
            Self::AdmissionWrite
            | Self::CommitBeforeCall
            | Self::DeliveryWrite
            | Self::Checkpoint
            | Self::CacheCommitBeforeCall => "before",
            Self::InferenceExit
            | Self::CommitAfterReturn
            | Self::StdoutWrite
            | Self::MigrationPublish
            | Self::CacheCommitAfterReturn => "after",
        }
    }

    const fn is_worker(self) -> bool {
        matches!(self, Self::Checkpoint | Self::MigrationPublish)
    }

    const fn is_lifecycle(self) -> bool {
        matches!(
            self,
            Self::CacheCommitBeforeCall | Self::CacheCommitAfterReturn
        )
    }
}

fn expected_cells() -> BTreeSet<(AgentProvider, MatrixFault)> {
    PROVIDERS
        .into_iter()
        .flat_map(|provider| FAULTS.into_iter().map(move |fault| (provider, fault)))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedStdout {
    NativeFallback,
    NoOutput,
    ClosedPipe,
    ExactResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedOutcome {
    Success,
    Abnormal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedCell {
    states: &'static [&'static str],
    attempts: usize,
    decisions: usize,
    commits: usize,
    attempt_state: Option<&'static str>,
    delivery: Option<&'static str>,
    stdout: ExpectedStdout,
    outcome: ExpectedOutcome,
}

fn expected_cell(fault: MatrixFault) -> ExpectedCell {
    match fault {
        MatrixFault::AdmissionWrite => ExpectedCell {
            states: &[],
            attempts: 0,
            decisions: 0,
            commits: 0,
            attempt_state: None,
            delivery: None,
            stdout: ExpectedStdout::NativeFallback,
            outcome: ExpectedOutcome::Success,
        },
        MatrixFault::InferenceExit => ExpectedCell {
            states: &["observed", "evaluating", "error"],
            attempts: 1,
            decisions: 0,
            commits: 0,
            attempt_state: Some("needs_input"),
            delivery: None,
            stdout: ExpectedStdout::NativeFallback,
            outcome: ExpectedOutcome::Success,
        },
        MatrixFault::CommitBeforeCall => ExpectedCell {
            states: &["observed", "evaluating"],
            attempts: 1,
            decisions: 0,
            commits: 0,
            attempt_state: Some("evaluating"),
            delivery: None,
            stdout: ExpectedStdout::NoOutput,
            outcome: ExpectedOutcome::Abnormal,
        },
        MatrixFault::CommitAfterReturn => ExpectedCell {
            states: &["observed", "evaluating", "allowed"],
            attempts: 1,
            decisions: 1,
            commits: 1,
            attempt_state: Some("decided"),
            delivery: Some("pending"),
            stdout: ExpectedStdout::NoOutput,
            outcome: ExpectedOutcome::Abnormal,
        },
        MatrixFault::StdoutWrite => ExpectedCell {
            states: &["observed", "evaluating", "allowed", "delivery_failed"],
            attempts: 1,
            decisions: 1,
            commits: 1,
            attempt_state: Some("decided"),
            delivery: Some("failed"),
            stdout: ExpectedStdout::ClosedPipe,
            outcome: ExpectedOutcome::Success,
        },
        MatrixFault::DeliveryWrite => ExpectedCell {
            states: &["observed", "evaluating", "allowed"],
            attempts: 1,
            decisions: 1,
            commits: 1,
            attempt_state: Some("decided"),
            delivery: Some("pending"),
            stdout: ExpectedStdout::ExactResponse,
            outcome: ExpectedOutcome::Success,
        },
        MatrixFault::Checkpoint
        | MatrixFault::MigrationPublish
        | MatrixFault::CacheCommitBeforeCall
        | MatrixFault::CacheCommitAfterReturn => {
            panic!("composite cells do not use the hook-only expected table")
        }
    }
}

fn native_fallback(provider: AgentProvider) -> &'static [u8] {
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => b"",
        AgentProvider::Antigravity => br#"{"decision":"ask","reason":"Coding Brain abstained"}"#,
    }
}

fn exact_response(provider: AgentProvider) -> &'static [u8] {
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => {
            br#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        }
        AgentProvider::Antigravity => br#"{"decision":"allow"}"#,
    }
}

#[derive(Debug)]
struct ProcessResult {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct FaultHarness {
    _temp: TempDir,
    home: PathBuf,
    config_base: PathBuf,
    state_base: PathBuf,
    state_root: PathBuf,
    capability: NamedTempFile,
    nonce: String,
    marker_reader: File,
    marker_writer: Option<File>,
    child: Option<Child>,
    started_at_ms: i64,
}

impl Drop for FaultHarness {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

impl FaultHarness {
    fn new(fault: MatrixFault) -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let temp_root = fs::canonicalize(temp.path()).unwrap();
        let home = temp_root.join("home");
        let config_base = temp_root.join("config");
        let state_base = temp_root.join("state");
        let state_root = state_base.join("coding-brain");
        for directory in [&home, &config_base, &state_base, &state_root] {
            fs::create_dir_all(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let fifo = temp_root.join("control.fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let marker_reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        let marker_writer = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&fifo)
            .unwrap();
        let metadata = marker_writer.metadata().unwrap();

        let capability = NamedTempFile::new_in(&temp_root).unwrap();
        let nonce = capability
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        serde_json::to_writer(
            capability.as_file(),
            &json!({
                "version": 1,
                "state_root": fs::canonicalize(&state_root).unwrap(),
                "nonce": nonce,
                "selection": {"kind": "matrix", "selection": fault.as_cli_value()},
                "control_device": metadata.dev(),
                "control_inode": metadata.ino(),
            }),
        )
        .unwrap();
        capability.as_file().sync_all().unwrap();
        fs::set_permissions(capability.path(), fs::Permissions::from_mode(0o600)).unwrap();

        Self {
            _temp: temp,
            home,
            config_base,
            state_base,
            state_root,
            capability,
            nonce,
            marker_reader,
            marker_writer: Some(marker_writer),
            child: None,
            started_at_ms: epoch_ms(),
        }
    }

    fn prepare_current_storage(&self) {
        assert_eq!(
            MigrationCoordinator::at(&self.state_root)
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete
        );
    }

    fn install_model(&self, fail: bool) {
        let config = self.config_base.join("coding-brain/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            config,
            "[brain]\nenabled = true\nendpoint = \"http://brain.example.test/api/generate\"\n",
        )
        .unwrap();
        let gate = self.state_root.join("brain/gate-mode");
        fs::create_dir_all(gate.parent().unwrap()).unwrap();
        fs::write(gate, b"auto\n").unwrap();
        let bin = self.home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        let script = if fail {
            "#!/bin/sh\nset -eu\ndd of=/dev/null 2>/dev/null\nexit 28\n".to_owned()
        } else {
            let suggestion = json!({
                "action": "approve",
                "message": null,
                "reasoning": "matrix fixture decision",
                "confidence": 0.99,
            })
            .to_string();
            let response = json!({"response": suggestion}).to_string();
            format!("#!/bin/sh\nset -eu\ndd of=/dev/null 2>/dev/null\nprintf '%s' '{response}'\n")
        };
        fs::write(&curl, script).unwrap();
        fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn seed_antigravity(&self) {
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Antigravity,
            "agy-conversation-1".into(),
            Some("invocation-1".into()),
            Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
            self.home.clone(),
        )
        .unwrap();
        let event = LifecycleEvent::from_parts_with_turn_initial_step(
            identity,
            LifecycleEventKind::UserPromptSubmit,
            Some(5),
        )
        .unwrap();
        let mut database = BrainDb::open_current(
            &StoragePaths::at(&self.state_root),
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(2)),
        )
        .unwrap();
        assert_eq!(
            database.record_lifecycle(event, 1).unwrap().outcome,
            ApplyOutcome::Applied
        );
    }

    fn payload(&self, provider: AgentProvider) -> Vec<u8> {
        match provider {
            AgentProvider::Codex => serde_json::to_vec(&json!({
                "session_id": "session-1",
                "turn_id": "turn-1",
                "transcript_path": "/tmp/rollout-matrix.jsonl",
                "cwd": self.home,
                "hook_event_name": "PermissionRequest",
                "permission_mode": "default",
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
            }))
            .unwrap(),
            AgentProvider::Claude => {
                let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
                    "fixtures/hooks/claude-permission-request.json"
                ))
                .unwrap();
                value["cwd"] = json!(self.home);
                serde_json::to_vec(&value).unwrap()
            }
            AgentProvider::Antigravity => {
                let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
                    "fixtures/hooks/antigravity-pre-tool-use.json"
                ))
                .unwrap();
                value["workspacePaths"] = json!([self.home]);
                serde_json::to_vec(&value).unwrap()
            }
        }
    }

    fn command_base(&self) -> Command {
        let mut path = vec![self.home.join("bin")];
        if let Some(existing) = std::env::var_os("PATH") {
            path.extend(std::env::split_paths(&existing));
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_base)
            .env("XDG_STATE_HOME", &self.state_base)
            .env("PATH", std::env::join_paths(path).unwrap())
            .current_dir(&self.home);
        command
    }

    fn arm_command(&mut self, command: &mut Command, fault: MatrixFault) {
        let writer = self.marker_writer.as_ref().unwrap();
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
            0
        );
        command
            .arg(if fault.is_worker() {
                "--fault-worker"
            } else if fault.is_lifecycle() {
                "--lifecycle-hook"
            } else {
                "--permission-hook"
            })
            .args(["--fault-point", fault.as_cli_value()])
            .arg("--fault-capability")
            .arg(self.capability.path())
            .args(["--fault-nonce", &self.nonce])
            .args(["--fault-control-fd", &writer.as_raw_fd().to_string()]);
    }

    fn spawn_armed_hook(&mut self, provider: AgentProvider, fault: MatrixFault) {
        let mut command = self.command_base();
        self.arm_command(&mut command, fault);
        command.args(["--provider", provider.as_str()]);
        if provider == AgentProvider::Antigravity {
            command.args(["--antigravity-hook-event", "PreToolUse"]);
        }
        command.stdin(Stdio::piped()).stderr(Stdio::piped());
        if fault == MatrixFault::StdoutWrite {
            command.stdout(readerless_pipe_writer());
        } else {
            command.stdout(Stdio::piped());
        }
        let child = command.spawn().unwrap();
        self.child = Some(child);
        self.marker_writer.take();
        let write_result = match self.child.as_mut().and_then(|child| child.stdin.take()) {
            Some(mut stdin) => stdin.write_all(&self.payload(provider)),
            None => Err(std::io::Error::other("armed hook stdin pipe is missing")),
        };
        if let Err(error) = write_result {
            self.kill_and_reap();
            panic!("armed hook stdin write failed: {error}");
        }
    }

    fn spawn_armed_worker(&mut self, fault: MatrixFault) {
        let mut command = self.command_base();
        self.arm_command(&mut command, fault);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.child = Some(command.spawn().unwrap());
        self.marker_writer.take();
    }

    fn spawn_armed_lifecycle(&mut self, fault: MatrixFault) {
        let mut command = self.command_base();
        self.arm_command(&mut command, fault);
        command
            .args(["--provider", "codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.child = Some(command.spawn().unwrap());
        self.marker_writer.take();
        let payload = serde_json::to_vec(&json!({
            "session_id": "cache-fault-session",
            "turn_id": "cache-fault-turn",
            "transcript_path": "/tmp/cache-fault-rollout.jsonl",
            "cwd": self.home,
            "hook_event_name": "UserPromptSubmit",
            "prompt": "cache fault payload",
        }))
        .unwrap();
        self.child
            .as_mut()
            .unwrap()
            .stdin
            .take()
            .unwrap()
            .write_all(&payload)
            .unwrap();
    }

    fn finish_armed(&mut self, fault: MatrixFault) -> ProcessResult {
        self.finish_armed_with_deadlines(fault, Duration::from_secs(2), Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn finish_armed_with_deadlines(
        &mut self,
        fault: MatrixFault,
        marker_timeout: Duration,
        exit_timeout: Duration,
    ) -> Result<ProcessResult, String> {
        let expected = format!(
            "CBRAIN-FAULT-V1\0{}\0{}\0-\n",
            fault.as_cli_value(),
            fault.marker_position(),
        );
        let marker_deadline = Instant::now() + marker_timeout;
        let mut frame = [0_u8; 513];
        loop {
            let remaining = marker_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let output = self.kill_and_collect_output();
                return Err(format!(
                    "marker deadline expired for {fault:?}; child output: {output:?}"
                ));
            }
            let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
            let mut descriptor = libc::pollfd {
                fd: self.marker_reader.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if ready == -1 {
                let error = std::io::Error::last_os_error();
                self.kill_and_reap();
                return Err(format!("marker poll failed: {error}"));
            }
            if ready == 0 {
                continue;
            }
            match self.marker_reader.read(&mut frame) {
                Ok(length) => {
                    if &frame[..length] != expected.as_bytes() {
                        self.kill_and_reap();
                        return Err(format!(
                            "unexpected marker for {fault:?}: {:?}",
                            &frame[..length]
                        ));
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    self.kill_and_reap();
                    return Err(format!("marker read failed: {error}"));
                }
            }
        }

        let output = self.collect_child_with_deadline(
            exit_timeout,
            &format!("process exit deadline expired for {fault:?}"),
            "armed",
        )?;
        let mut extra = Vec::new();
        self.marker_reader
            .read_to_end(&mut extra)
            .map_err(|error| format!("marker EOF read failed: {error}"))?;
        if !extra.is_empty() {
            return Err(format!("more than one marker frame for {fault:?}"));
        }
        Ok(ProcessResult {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn collect_child_with_deadline(
        &mut self,
        exit_timeout: Duration,
        deadline_error: &str,
        child_kind: &str,
    ) -> Result<Output, String> {
        let exit_deadline = Instant::now() + exit_timeout;
        loop {
            let status = match self.child.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(status) => status,
                    Err(error) => {
                        self.kill_and_reap();
                        return Err(format!("{child_kind} child wait failed: {error}"));
                    }
                },
                None => return Err(format!("{child_kind} child ownership was lost")),
            };
            if status.is_some() {
                break;
            }
            if Instant::now() >= exit_deadline {
                self.kill_and_reap();
                return Err(deadline_error.to_owned());
            }
            std::thread::yield_now();
        }
        let child = self.child.take().unwrap();
        child
            .wait_with_output()
            .map_err(|error| format!("{child_kind} child output collection failed: {error}"))
    }

    fn kill_and_reap(&mut self) {
        let _ = self.kill_and_collect_output();
    }

    fn kill_and_collect_output(&mut self) -> Option<Output> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            return child.wait_with_output().ok();
        }
        None
    }

    fn run_unarmed_hook(&mut self, provider: AgentProvider) -> Output {
        let payload = self.payload(provider);
        let mut command = self.command_base();
        command.args(["--permission-hook", "--provider", provider.as_str()]);
        if provider == AgentProvider::Antigravity {
            command.args(["--antigravity-hook-event", "PreToolUse"]);
        }
        self.child = Some(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let write_result = match self.child.as_mut().and_then(|child| child.stdin.take()) {
            Some(mut stdin) => stdin.write_all(&payload),
            None => Err(std::io::Error::other("restart hook stdin pipe is missing")),
        };
        if let Err(error) = write_result {
            self.kill_and_reap();
            panic!("restart hook stdin write failed: {error}");
        }
        self.finish_unarmed_with_deadline(Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn run_unarmed_process(&mut self) -> Output {
        let mut command = self.command_base();
        command
            .args(["config", "validate"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.child = Some(command.spawn().unwrap());
        self.finish_unarmed_with_deadline(Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn finish_unarmed_with_deadline(&mut self, exit_timeout: Duration) -> Result<Output, String> {
        self.collect_child_with_deadline(
            exit_timeout,
            "restart process exit deadline expired",
            "restart",
        )
    }

    fn assert_no_second_marker(&mut self) {
        let mut extra = Vec::new();
        assert_eq!(self.marker_reader.read_to_end(&mut extra).unwrap(), 0);
        assert!(extra.is_empty());
    }
}

fn readerless_pipe_writer() -> Stdio {
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    let reader = unsafe { File::from_raw_fd(descriptors[0]) };
    let writer = unsafe { File::from_raw_fd(descriptors[1]) };
    drop(reader);
    Stdio::from(writer)
}

fn epoch_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn assert_outcome(result: &ProcessResult, expected: ExpectedOutcome) {
    match expected {
        ExpectedOutcome::Success => assert!(
            result.status.success(),
            "status {:?}\nstderr:\n{}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        ),
        ExpectedOutcome::Abnormal => assert_eq!(result.status.signal(), Some(libc::SIGABRT)),
    }
}

fn assert_stdout(result: &ProcessResult, provider: AgentProvider, expected: ExpectedStdout) {
    match expected {
        ExpectedStdout::NativeFallback => assert_eq!(result.stdout, native_fallback(provider)),
        ExpectedStdout::NoOutput | ExpectedStdout::ClosedPipe => assert!(result.stdout.is_empty()),
        ExpectedStdout::ExactResponse => assert_eq!(result.stdout, exact_response(provider)),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptRow {
    attempt_id: String,
    request_identity_key: String,
    provider: String,
    session_id: String,
    provider_session_id: Option<String>,
    turn_id: String,
    tool_use_id: Option<String>,
    request_key: String,
    cwd: Vec<u8>,
    project_id: Vec<u8>,
    tool_name: String,
    activity_id: String,
    authority_action: Option<String>,
    attempt_state: String,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivityRow {
    source_cursor: i64,
    activity_id: String,
    event_state: String,
    recorded_at_ms: i64,
    permission_attempt_id: String,
    terminal_provider: Option<String>,
    terminal_session_id: Option<String>,
    terminal_turn_id: Option<String>,
    terminal_tool_use_id: Option<String>,
    terminal_action: Option<String>,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecisionRow {
    decision_id: String,
    permission_attempt_id: String,
    provider: String,
    session_id: String,
    turn_id: String,
    tool_use_id: Option<String>,
    authority_action: String,
    decision_source: String,
    decided_at_ms: i64,
    source_cursor: i64,
    normalized_command: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommitRow {
    attempt_id: String,
    transaction_id: String,
    decision_id: String,
    terminal_activity_id: String,
    authority_action: String,
    evidence_kind: String,
    delivery_state: String,
    response_eligible: i64,
    committed_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistedSnapshot {
    attempts: Vec<AttemptRow>,
    activities: Vec<ActivityRow>,
    decisions: Vec<DecisionRow>,
    commits: Vec<CommitRow>,
}

struct SnapshotExpectation<'a> {
    states: &'a [&'a str],
    attempt_states: &'a [&'a str],
    decisions: usize,
    commits: usize,
    delivery: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct ProviderIdentity {
    session_id: &'static str,
    turn_id: &'static str,
    tool_use_id: Option<&'static str>,
    tool_name: &'static str,
}

struct ExactRequestKeys {
    request_key: String,
    request_identity_key: String,
    project_id: Vec<u8>,
}

fn provider_identity(provider: AgentProvider) -> ProviderIdentity {
    match provider {
        AgentProvider::Codex => ProviderIdentity {
            session_id: "session-1",
            turn_id: "turn-1",
            tool_use_id: None,
            tool_name: "Bash",
        },
        AgentProvider::Claude => ProviderIdentity {
            session_id: "claude-session-1",
            turn_id: "claude-session-1",
            tool_use_id: None,
            tool_name: "Bash",
        },
        AgentProvider::Antigravity => ProviderIdentity {
            session_id: "agy-conversation-1",
            turn_id: "step-5",
            tool_use_id: Some("step-5"),
            tool_name: "run_command",
        },
    }
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn hash_optional(hash: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, value);
        }
        None => hash.update([0]),
    }
}

fn exact_request_keys(harness: &FaultHarness, provider: AgentProvider) -> ExactRequestKeys {
    let identity = provider_identity(provider);
    let payload: serde_json::Value = serde_json::from_slice(&harness.payload(provider)).unwrap();
    let tool_input = match provider {
        AgentProvider::Codex | AgentProvider::Claude => &payload["tool_input"],
        AgentProvider::Antigravity => &payload["toolCall"]["args"],
    };
    let mut request_hash = Sha256::new();
    hash_field(&mut request_hash, b"coding-brain:permission-request-key:v1");
    hash_field(&mut request_hash, provider.as_str().as_bytes());
    match identity.tool_use_id {
        Some(tool_use_id) => {
            hash_field(&mut request_hash, b"tool-use-id");
            hash_field(&mut request_hash, tool_use_id.as_bytes());
        }
        None => hash_field(&mut request_hash, b"no-tool-use-id"),
    }
    hash_field(&mut request_hash, identity.tool_name.as_bytes());
    hash_field(&mut request_hash, &serde_json::to_vec(tool_input).unwrap());
    let request_key = format!("{:x}", request_hash.finalize());

    let paths = CodingBrainPaths::resolve(&PathEnvironment::new(
        Some(harness.config_base.clone()),
        Some(harness.state_base.clone()),
        Some(harness.home.clone()),
    ))
    .unwrap();
    let project_id =
        serde_json::to_vec(ProjectIdentity::load(&harness.home, &paths).unwrap().id()).unwrap();
    let mut identity_hash = Sha256::new();
    hash_field(
        &mut identity_hash,
        b"coding-brain.sqlite-permission-request.v1",
    );
    hash_field(&mut identity_hash, provider.as_str().as_bytes());
    hash_field(&mut identity_hash, identity.session_id.as_bytes());
    hash_optional(&mut identity_hash, None);
    hash_field(&mut identity_hash, identity.turn_id.as_bytes());
    hash_field(&mut identity_hash, harness.home.as_os_str().as_bytes());
    hash_field(&mut identity_hash, request_key.as_bytes());
    hash_optional(&mut identity_hash, identity.tool_use_id.map(str::as_bytes));
    hash_field(&mut identity_hash, identity.tool_name.as_bytes());
    hash_field(&mut identity_hash, &project_id);

    ExactRequestKeys {
        request_key,
        request_identity_key: format!("{:x}", identity_hash.finalize()),
        project_id,
    }
}

impl PersistedSnapshot {
    fn read(harness: &FaultHarness, provider: AgentProvider) -> Self {
        let identity = provider_identity(provider);
        let connection = Connection::open(harness.state_root.join("db/brain.sqlite3")).unwrap();
        let mut attempts_statement = connection
            .prepare(
                "SELECT attempt_id, request_identity_key, provider, session_id,
                        provider_session_id, turn_id, tool_use_id, request_key, cwd, project_id, tool_name,
                        activity_id, authority_action, attempt_state, created_at_ms, updated_at_ms
                 FROM permission_attempts
                 WHERE provider = ?1 AND session_id = ?2 AND turn_id = ?3
                   AND tool_use_id IS ?4
                 ORDER BY created_at_ms, attempt_id",
            )
            .unwrap();
        let attempts = attempts_statement
            .query_map(
                params![
                    provider.as_str(),
                    identity.session_id,
                    identity.turn_id,
                    identity.tool_use_id,
                ],
                |row| {
                    Ok(AttemptRow {
                        attempt_id: row.get(0)?,
                        request_identity_key: row.get(1)?,
                        provider: row.get(2)?,
                        session_id: row.get(3)?,
                        provider_session_id: row.get(4)?,
                        turn_id: row.get(5)?,
                        tool_use_id: row.get(6)?,
                        request_key: row.get(7)?,
                        cwd: row.get(8)?,
                        project_id: row.get(9)?,
                        tool_name: row.get(10)?,
                        activity_id: row.get(11)?,
                        authority_action: row.get(12)?,
                        attempt_state: row.get(13)?,
                        created_at_ms: row.get(14)?,
                        updated_at_ms: row.get(15)?,
                    })
                },
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut activity_statement = connection
            .prepare(
                "SELECT e.source_cursor, e.activity_id, e.event_state, e.recorded_at_ms,
                        e.permission_attempt_id, e.terminal_provider, e.terminal_session_id,
                        e.terminal_turn_id, e.terminal_tool_use_id, e.terminal_action,
                        e.event_payload
                 FROM activity_events e
                 JOIN permission_attempts a ON a.attempt_id = e.permission_attempt_id
                 WHERE a.provider = ?1 AND a.session_id = ?2 AND a.turn_id = ?3
                   AND a.tool_use_id IS ?4
                 ORDER BY e.source_cursor",
            )
            .unwrap();
        let activities = activity_statement
            .query_map(
                params![
                    provider.as_str(),
                    identity.session_id,
                    identity.turn_id,
                    identity.tool_use_id,
                ],
                |row| {
                    Ok(ActivityRow {
                        source_cursor: row.get(0)?,
                        activity_id: row.get(1)?,
                        event_state: row.get(2)?,
                        recorded_at_ms: row.get(3)?,
                        permission_attempt_id: row.get(4)?,
                        terminal_provider: row.get(5)?,
                        terminal_session_id: row.get(6)?,
                        terminal_turn_id: row.get(7)?,
                        terminal_tool_use_id: row.get(8)?,
                        terminal_action: row.get(9)?,
                        payload: row.get(10)?,
                    })
                },
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut decision_statement = connection
            .prepare(
                "SELECT d.decision_id, d.permission_attempt_id, d.provider, d.session_id,
                        d.turn_id, d.tool_use_id, d.authority_action, d.decision_source,
                        d.decided_at_ms, p.source_cursor, p.normalized_command
                 FROM decision_identities d
                 JOIN permission_attempts a ON a.attempt_id = d.permission_attempt_id
                 JOIN decision_payloads p ON p.decision_id = d.decision_id
                 WHERE a.provider = ?1 AND a.session_id = ?2 AND a.turn_id = ?3
                   AND a.tool_use_id IS ?4
                 ORDER BY d.decided_at_ms, d.decision_id",
            )
            .unwrap();
        let decisions = decision_statement
            .query_map(
                params![
                    provider.as_str(),
                    identity.session_id,
                    identity.turn_id,
                    identity.tool_use_id,
                ],
                |row| {
                    Ok(DecisionRow {
                        decision_id: row.get(0)?,
                        permission_attempt_id: row.get(1)?,
                        provider: row.get(2)?,
                        session_id: row.get(3)?,
                        turn_id: row.get(4)?,
                        tool_use_id: row.get(5)?,
                        authority_action: row.get(6)?,
                        decision_source: row.get(7)?,
                        decided_at_ms: row.get(8)?,
                        source_cursor: row.get(9)?,
                        normalized_command: row.get(10)?,
                    })
                },
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut commit_statement = connection
            .prepare(
                "SELECT c.attempt_id, c.transaction_id, c.decision_id,
                        c.terminal_activity_id, c.authority_action, c.evidence_kind,
                        c.delivery_state, c.response_eligible, c.committed_at_ms
                 FROM permission_commits c
                 JOIN permission_attempts a ON a.attempt_id = c.attempt_id
                 WHERE a.provider = ?1 AND a.session_id = ?2 AND a.turn_id = ?3
                   AND a.tool_use_id IS ?4
                 ORDER BY c.committed_at_ms, c.attempt_id",
            )
            .unwrap();
        let commits = commit_statement
            .query_map(
                params![
                    provider.as_str(),
                    identity.session_id,
                    identity.turn_id,
                    identity.tool_use_id,
                ],
                |row| {
                    Ok(CommitRow {
                        attempt_id: row.get(0)?,
                        transaction_id: row.get(1)?,
                        decision_id: row.get(2)?,
                        terminal_activity_id: row.get(3)?,
                        authority_action: row.get(4)?,
                        evidence_kind: row.get(5)?,
                        delivery_state: row.get(6)?,
                        response_eligible: row.get(7)?,
                        committed_at_ms: row.get(8)?,
                    })
                },
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        Self {
            attempts,
            activities,
            decisions,
            commits,
        }
    }

    fn assert_exact(
        &self,
        harness: &FaultHarness,
        provider: AgentProvider,
        expected: SnapshotExpectation<'_>,
    ) {
        let identity = provider_identity(provider);
        let exact_keys = exact_request_keys(harness, provider);
        assert_eq!(self.attempts.len(), expected.attempt_states.len());
        assert_eq!(self.decisions.len(), expected.decisions);
        assert_eq!(self.commits.len(), expected.commits);
        assert_eq!(
            self.activities
                .iter()
                .map(|row| row.event_state.as_str())
                .collect::<Vec<_>>(),
            expected.states
        );
        assert_eq!(
            self.attempts
                .iter()
                .map(|row| row.attempt_state.as_str())
                .collect::<Vec<_>>(),
            expected.attempt_states
        );

        let finished_at_ms = epoch_ms();
        let attempt_ids = self
            .attempts
            .iter()
            .map(|row| row.attempt_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(attempt_ids.len(), self.attempts.len());
        let activity_ids = self
            .attempts
            .iter()
            .map(|row| row.activity_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(activity_ids.len(), self.attempts.len());

        for attempt in &self.attempts {
            assert!(!attempt.attempt_id.is_empty());
            assert_eq!(
                attempt.request_identity_key,
                exact_keys.request_identity_key
            );
            assert_eq!(attempt.request_key, exact_keys.request_key);
            assert_eq!(attempt.provider, provider.as_str());
            assert_eq!(attempt.session_id, identity.session_id);
            assert_eq!(attempt.provider_session_id, None);
            assert_eq!(attempt.turn_id, identity.turn_id);
            assert_eq!(attempt.tool_use_id.as_deref(), identity.tool_use_id);
            assert_eq!(attempt.cwd, harness.home.as_os_str().as_bytes());
            assert_eq!(attempt.project_id, exact_keys.project_id);
            assert_eq!(attempt.tool_name, identity.tool_name);
            assert!((harness.started_at_ms..=finished_at_ms).contains(&attempt.created_at_ms));
            assert!((attempt.created_at_ms..=finished_at_ms).contains(&attempt.updated_at_ms));
            assert_eq!(
                attempt.authority_action.as_deref(),
                (attempt.attempt_state == "decided").then_some("allow")
            );
        }
        for pair in self.activities.windows(2) {
            assert!(pair[0].source_cursor < pair[1].source_cursor);
            assert!(pair[0].recorded_at_ms <= pair[1].recorded_at_ms);
        }
        for activity in &self.activities {
            assert!(attempt_ids.contains(activity.permission_attempt_id.as_str()));
            let attempt = self
                .attempts
                .iter()
                .find(|row| row.attempt_id == activity.permission_attempt_id)
                .unwrap();
            assert_eq!(activity.activity_id, attempt.activity_id);
            assert!((harness.started_at_ms..=finished_at_ms).contains(&activity.recorded_at_ms));
            let payload: ActivityEvent = serde_json::from_slice(&activity.payload).unwrap();
            assert_eq!(payload.activity_id, activity.activity_id);
            assert_eq!(
                serde_json::to_value(payload.state).unwrap(),
                serde_json::Value::String(activity.event_state.clone())
            );
            let session = payload.session.unwrap();
            assert_eq!(session.provider, provider);
            assert_eq!(session.session_id, identity.session_id);
            assert_eq!(session.turn_id.as_deref(), Some(identity.turn_id));
            assert_eq!(session.tool_use_id.as_deref(), identity.tool_use_id);
            assert_eq!(payload.tool.as_deref(), Some(identity.tool_name));
            if activity.event_state == "allowed" {
                assert_eq!(
                    activity.terminal_provider.as_deref(),
                    Some(provider.as_str())
                );
                assert_eq!(
                    activity.terminal_session_id.as_deref(),
                    Some(identity.session_id)
                );
                assert_eq!(activity.terminal_turn_id.as_deref(), Some(identity.turn_id));
                assert_eq!(
                    activity.terminal_tool_use_id.as_deref(),
                    identity.tool_use_id
                );
                assert_eq!(activity.terminal_action.as_deref(), Some("allow"));
            } else {
                assert_eq!(activity.terminal_provider, None);
                assert_eq!(activity.terminal_session_id, None);
                assert_eq!(activity.terminal_turn_id, None);
                assert_eq!(activity.terminal_tool_use_id, None);
                assert_eq!(activity.terminal_action, None);
            }
        }

        let decision_ids = self
            .decisions
            .iter()
            .map(|row| row.decision_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(decision_ids.len(), self.decisions.len());
        for decision in &self.decisions {
            assert!(!decision.decision_id.is_empty());
            assert!(attempt_ids.contains(decision.permission_attempt_id.as_str()));
            assert_eq!(decision.provider, provider.as_str());
            assert_eq!(decision.session_id, identity.session_id);
            assert_eq!(decision.turn_id, identity.turn_id);
            assert_eq!(decision.tool_use_id.as_deref(), identity.tool_use_id);
            assert_eq!(decision.authority_action, "allow");
            assert_eq!(decision.decision_source, "model");
            assert_eq!(decision.normalized_command, "cargo test");
            assert!((harness.started_at_ms..=finished_at_ms).contains(&decision.decided_at_ms));
            let source = self
                .activities
                .iter()
                .find(|row| row.source_cursor == decision.source_cursor)
                .unwrap();
            assert_eq!(source.event_state, "allowed");
        }
        for commit in &self.commits {
            assert!(attempt_ids.contains(commit.attempt_id.as_str()));
            assert!(decision_ids.contains(commit.decision_id.as_str()));
            let attempt = self
                .attempts
                .iter()
                .find(|row| row.attempt_id == commit.attempt_id)
                .unwrap();
            assert_eq!(commit.terminal_activity_id, attempt.activity_id);
            assert!(!commit.transaction_id.is_empty());
            assert_eq!(commit.authority_action, "allow");
            assert_eq!(commit.evidence_kind, "provider_authority");
            assert_eq!(commit.response_eligible, 1);
            assert_eq!(commit.delivery_state, expected.delivery.unwrap());
            assert!((harness.started_at_ms..=finished_at_ms).contains(&commit.committed_at_ms));
        }

        for attempt in &self.attempts {
            let attempt_activities = self
                .activities
                .iter()
                .filter(|row| row.permission_attempt_id == attempt.attempt_id)
                .collect::<Vec<_>>();
            assert!(!attempt_activities.is_empty());
            assert!(attempt.created_at_ms <= attempt_activities[0].recorded_at_ms);
            for pair in attempt_activities.windows(2) {
                assert!(pair[0].source_cursor < pair[1].source_cursor);
                assert!(pair[0].recorded_at_ms <= pair[1].recorded_at_ms);
            }
            let attempt_decisions = self
                .decisions
                .iter()
                .filter(|row| row.permission_attempt_id == attempt.attempt_id)
                .collect::<Vec<_>>();
            let attempt_commits = self
                .commits
                .iter()
                .filter(|row| row.attempt_id == attempt.attempt_id)
                .collect::<Vec<_>>();
            if attempt.attempt_state == "decided" {
                assert_eq!(attempt_decisions.len(), 1);
                assert_eq!(attempt_commits.len(), 1);
                let decision = attempt_decisions[0];
                let commit = attempt_commits[0];
                let allowed = attempt_activities
                    .iter()
                    .find(|row| row.event_state == "allowed")
                    .unwrap();
                assert_eq!(decision.permission_attempt_id, attempt.attempt_id);
                assert_eq!(decision.source_cursor, allowed.source_cursor);
                assert_eq!(allowed.permission_attempt_id, attempt.attempt_id);
                assert_eq!(allowed.activity_id, attempt.activity_id);
                assert_eq!(attempt.updated_at_ms, allowed.recorded_at_ms);
                assert_eq!(decision.decided_at_ms, allowed.recorded_at_ms);
                assert_eq!(commit.attempt_id, attempt.attempt_id);
                assert_eq!(commit.decision_id, decision.decision_id);
                assert_eq!(commit.terminal_activity_id, allowed.activity_id);
                assert_eq!(
                    commit.transaction_id,
                    format!("sqlite-transaction-{}", decision.decision_id)
                );
                assert_eq!(commit.committed_at_ms, decision.decided_at_ms);
                let deliveries = attempt_activities
                    .iter()
                    .filter(|row| {
                        matches!(row.event_state.as_str(), "delivered" | "delivery_failed")
                    })
                    .collect::<Vec<_>>();
                match commit.delivery_state.as_str() {
                    "pending" => assert!(deliveries.is_empty()),
                    "delivered" => {
                        assert_eq!(deliveries.len(), 1);
                        assert_eq!(deliveries[0].event_state, "delivered");
                        assert!(commit.committed_at_ms <= deliveries[0].recorded_at_ms);
                    }
                    "failed" => {
                        assert_eq!(deliveries.len(), 1);
                        assert_eq!(deliveries[0].event_state, "delivery_failed");
                        assert!(commit.committed_at_ms <= deliveries[0].recorded_at_ms);
                    }
                    state => panic!("unexpected delivery state {state}"),
                }
            } else {
                assert!(attempt_decisions.is_empty());
                assert!(attempt_commits.is_empty());
                assert!(attempt_activities.last().unwrap().recorded_at_ms <= attempt.updated_at_ms);
            }
        }
    }

    fn captured_ids(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            self.attempts
                .iter()
                .map(|row| row.attempt_id.clone())
                .collect(),
            self.decisions
                .iter()
                .map(|row| row.decision_id.clone())
                .collect(),
            self.activities
                .iter()
                .map(|row| row.activity_id.clone())
                .collect(),
        )
    }
}

fn assert_global_permission_counts(harness: &FaultHarness, snapshot: &PersistedSnapshot) {
    let connection = Connection::open(harness.state_root.join("db/brain.sqlite3")).unwrap();
    for (table, expected) in [
        ("permission_attempts", snapshot.attempts.len()),
        ("decision_identities", snapshot.decisions.len()),
        ("decision_payloads", snapshot.decisions.len()),
        ("permission_commits", snapshot.commits.len()),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(usize::try_from(count).unwrap(), expected, "{table}");
    }
}

fn run_hook_fault_case(provider: AgentProvider, fault: MatrixFault) {
    let expected = expected_cell(fault);
    let mut harness = FaultHarness::new(fault);
    harness.prepare_current_storage();
    harness.install_model(fault == MatrixFault::InferenceExit);
    if provider == AgentProvider::Antigravity {
        harness.seed_antigravity();
    }

    harness.spawn_armed_hook(provider, fault);
    let first_result = harness.finish_armed(fault);
    assert_outcome(&first_result, expected.outcome);
    assert_stdout(&first_result, provider, expected.stdout);
    if expected.outcome == ExpectedOutcome::Abnormal {
        assert_eq!(first_result.status.code(), None);
        assert!(first_result.stderr.is_empty());
    }

    let first = PersistedSnapshot::read(&harness, provider);
    let first_attempt_states = expected
        .attempt_state
        .map_or_else(Vec::new, |state| vec![state]);
    first.assert_exact(
        &harness,
        provider,
        SnapshotExpectation {
            states: expected.states,
            attempt_states: &first_attempt_states,
            decisions: expected.decisions,
            commits: expected.commits,
            delivery: expected.delivery,
        },
    );
    assert_eq!(first.attempts.len(), expected.attempts);
    assert_global_permission_counts(&harness, &first);
    let first_ids = first.captured_ids();

    if fault == MatrixFault::InferenceExit {
        harness.install_model(false);
    }
    let restart = harness.run_unarmed_hook(provider);
    assert!(
        restart.status.success(),
        "restart status {:?}\nstderr:\n{}",
        restart.status,
        String::from_utf8_lossy(&restart.stderr)
    );

    let after = PersistedSnapshot::read(&harness, provider);
    match fault {
        MatrixFault::AdmissionWrite => {
            assert_eq!(
                restart.stdout,
                exact_response(provider),
                "stderr:\n{}",
                String::from_utf8_lossy(&restart.stderr)
            );
            after.assert_exact(
                &harness,
                provider,
                SnapshotExpectation {
                    states: &["observed", "evaluating", "allowed", "delivered"],
                    attempt_states: &["decided"],
                    decisions: 1,
                    commits: 1,
                    delivery: Some("delivered"),
                },
            );
        }
        MatrixFault::CommitBeforeCall => {
            assert_eq!(restart.stdout, exact_response(provider));
            after.assert_exact(
                &harness,
                provider,
                SnapshotExpectation {
                    states: &[
                        "observed",
                        "evaluating",
                        "observed",
                        "evaluating",
                        "allowed",
                        "delivered",
                    ],
                    attempt_states: &["abandoned", "decided"],
                    decisions: 1,
                    commits: 1,
                    delivery: Some("delivered"),
                },
            );
            assert_eq!(after.attempts[0].attempt_id, first_ids.0[0]);
            assert_eq!(after.activities[0].activity_id, first_ids.2[0]);
            assert_eq!(after.activities[1].activity_id, first_ids.2[1]);
        }
        MatrixFault::InferenceExit
        | MatrixFault::CommitAfterReturn
        | MatrixFault::StdoutWrite
        | MatrixFault::DeliveryWrite => {
            assert_eq!(restart.stdout, native_fallback(provider));
            let after_ids = after.captured_ids();
            assert_eq!(after_ids, first_ids);
            assert_eq!(after, first);
        }
        MatrixFault::Checkpoint
        | MatrixFault::MigrationPublish
        | MatrixFault::CacheCommitBeforeCall
        | MatrixFault::CacheCommitAfterReturn => unreachable!(),
    }
    assert_global_permission_counts(&harness, &after);
    harness.assert_no_second_marker();
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

fn lifecycle_session_json(
    harness: &FaultHarness,
    provider: AgentProvider,
    session_id: &str,
) -> serde_json::Value {
    let database = BrainDb::open_current(
        &StoragePaths::at(&harness.state_root),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let snapshot = database.read_lifecycle().unwrap();
    serde_json::to_value(
        snapshot
            .sessions
            .get(&AgentSessionKey::native(provider, session_id).storage_key())
            .unwrap(),
    )
    .unwrap()
}

fn run_checkpoint_case(provider: AgentProvider) {
    let fault = MatrixFault::Checkpoint;
    let mut harness = FaultHarness::new(fault);
    harness.prepare_current_storage();
    harness.install_model(false);
    if provider == AgentProvider::Antigravity {
        harness.seed_antigravity();
    }

    let mut sentinel_database = BrainDb::open_current(
        &StoragePaths::at(&harness.state_root),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let sentinel_identity = LifecycleIdentity::try_new(
        provider,
        format!("checkpoint-sentinel-{}", provider.as_str()),
        Some("sentinel-turn".into()),
        None,
        harness.home.clone(),
    )
    .unwrap();
    let sentinel =
        LifecycleEvent::from_parts(sentinel_identity, LifecycleEventKind::UserPromptSubmit)
            .unwrap();
    assert_eq!(
        sentinel_database
            .record_lifecycle(sentinel, 1)
            .unwrap()
            .outcome,
        ApplyOutcome::Applied
    );
    let sentinel_session_id = format!("checkpoint-sentinel-{}", provider.as_str());
    let before = serde_json::to_value(
        sentinel_database
            .read_lifecycle()
            .unwrap()
            .sessions
            .get(&AgentSessionKey::native(provider, &sentinel_session_id).storage_key())
            .unwrap(),
    )
    .unwrap();
    let wal = harness.state_root.join("db/brain.sqlite3-wal");
    assert!(
        fs::metadata(&wal).unwrap().len() > 0,
        "sentinel did not reach WAL"
    );

    harness.spawn_armed_worker(fault);
    let worker = harness.finish_armed(fault);
    assert_eq!(worker.status.code(), Some(1));
    assert_eq!(worker.status.signal(), None);
    assert!(worker.stdout.is_empty());
    assert!(
        worker.stderr
            == b"Error: Custom { kind: Other, error: StorageFault { operation: Checkpoint, category: Io } }\n",
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    assert_eq!(
        serde_json::to_value(
            sentinel_database
                .read_lifecycle()
                .unwrap()
                .sessions
                .get(&AgentSessionKey::native(provider, &sentinel_session_id).storage_key())
                .unwrap()
        )
        .unwrap(),
        before
    );
    drop(sentinel_database);

    let hook = harness.run_unarmed_hook(provider);
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    assert_eq!(hook.stdout, exact_response(provider));
    let first = PersistedSnapshot::read(&harness, provider);
    first.assert_exact(
        &harness,
        provider,
        SnapshotExpectation {
            states: &["observed", "evaluating", "allowed", "delivered"],
            attempt_states: &["decided"],
            decisions: 1,
            commits: 1,
            delivery: Some("delivered"),
        },
    );
    assert_eq!(
        lifecycle_session_json(&harness, provider, &sentinel_session_id),
        before
    );
    let ids = first.captured_ids();

    let second = harness.run_unarmed_hook(provider);
    assert!(second.status.success());
    assert_eq!(second.stdout, native_fallback(provider));
    let after = PersistedSnapshot::read(&harness, provider);
    assert_eq!(after, first);
    assert_eq!(after.captured_ids(), ids);
    assert_eq!(
        lifecycle_session_json(&harness, provider, &sentinel_session_id),
        before
    );
    harness.assert_no_second_marker();
}

fn migration_generation(harness: &FaultHarness) -> i64 {
    Connection::open(harness.state_root.join("db/brain.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT migration_generation FROM schema_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn run_migration_case(provider: AgentProvider) {
    let fault = MatrixFault::MigrationPublish;
    let mut harness = FaultHarness::new(fault);
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/storage/legacy-v0.59.1"),
        &harness.state_root,
    );
    harness.install_model(false);

    harness.spawn_armed_worker(fault);
    let worker = harness.finish_armed(fault);
    assert_eq!(worker.status.signal(), Some(libc::SIGABRT));
    assert_eq!(worker.status.code(), None);
    assert!(worker.stdout.is_empty());
    assert!(worker.stderr.is_empty());
    assert_eq!(
        MigrationCoordinator::at(&harness.state_root)
            .inspect()
            .unwrap(),
        MigrationStatus::BrainPublishedIncomplete
    );

    let incomplete_hook = harness.run_unarmed_hook(provider);
    assert!(incomplete_hook.status.success());
    assert_eq!(incomplete_hook.stdout, native_fallback(provider));
    assert_eq!(
        incomplete_hook.stderr,
        b"cbrain permission hook: SQLite storage unavailable: SQLite storage migration is active\n"
    );
    let absent = PersistedSnapshot::read(&harness, provider);
    assert!(absent.attempts.is_empty());
    assert!(absent.activities.is_empty());
    assert!(absent.decisions.is_empty());
    assert!(absent.commits.is_empty());

    let resume = harness.run_unarmed_process();
    assert!(
        resume.status.success(),
        "{}",
        String::from_utf8_lossy(&resume.stderr)
    );
    assert_eq!(
        MigrationCoordinator::at(&harness.state_root)
            .inspect()
            .unwrap(),
        MigrationStatus::Complete
    );
    let generation = migration_generation(&harness);
    assert!(generation > 0);
    if provider == AgentProvider::Antigravity {
        harness.seed_antigravity();
    }
    let hook = harness.run_unarmed_hook(provider);
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    assert_eq!(hook.stdout, exact_response(provider));
    let first = PersistedSnapshot::read(&harness, provider);
    first.assert_exact(
        &harness,
        provider,
        SnapshotExpectation {
            states: &["observed", "evaluating", "allowed", "delivered"],
            attempt_states: &["decided"],
            decisions: 1,
            commits: 1,
            delivery: Some("delivered"),
        },
    );
    let ids = first.captured_ids();

    let unchanged = harness.run_unarmed_process();
    assert!(unchanged.status.success());
    assert_eq!(migration_generation(&harness), generation);
    let second = harness.run_unarmed_hook(provider);
    assert!(second.status.success());
    assert_eq!(second.stdout, native_fallback(provider));
    let after = PersistedSnapshot::read(&harness, provider);
    assert_eq!(after, first);
    assert_eq!(after.captured_ids(), ids);
    assert_eq!(migration_generation(&harness), generation);
    harness.assert_no_second_marker();
}

fn run_live_fault_case(provider: AgentProvider, fault: MatrixFault) {
    let _guard = LIVE_FAULT_MATRIX_LOCK.lock().unwrap();
    match fault {
        MatrixFault::Checkpoint => run_checkpoint_case(provider),
        MatrixFault::MigrationPublish => run_migration_case(provider),
        MatrixFault::CacheCommitBeforeCall | MatrixFault::CacheCommitAfterReturn => unreachable!(),
        _ => run_hook_fault_case(provider, fault),
    }
}

macro_rules! live_fault_case {
    ($name:ident, $cell:ident, $provider:expr, $fault:expr) => {
        const $cell: (AgentProvider, MatrixFault) = ($provider, $fault);

        #[test]
        fn $name() {
            run_live_fault_case($cell.0, $cell.1);
        }
    };
}

live_fault_case!(
    codex_admission_write,
    CODEX_ADMISSION_WRITE_CELL,
    AgentProvider::Codex,
    MatrixFault::AdmissionWrite
);
live_fault_case!(
    codex_inference_exit,
    CODEX_INFERENCE_EXIT_CELL,
    AgentProvider::Codex,
    MatrixFault::InferenceExit
);
live_fault_case!(
    codex_commit_before_call,
    CODEX_COMMIT_BEFORE_CALL_CELL,
    AgentProvider::Codex,
    MatrixFault::CommitBeforeCall
);
live_fault_case!(
    codex_commit_after_return,
    CODEX_COMMIT_AFTER_RETURN_CELL,
    AgentProvider::Codex,
    MatrixFault::CommitAfterReturn
);
live_fault_case!(
    codex_stdout_write,
    CODEX_STDOUT_WRITE_CELL,
    AgentProvider::Codex,
    MatrixFault::StdoutWrite
);
live_fault_case!(
    codex_delivery_write,
    CODEX_DELIVERY_WRITE_CELL,
    AgentProvider::Codex,
    MatrixFault::DeliveryWrite
);
live_fault_case!(
    codex_checkpoint,
    CODEX_CHECKPOINT_CELL,
    AgentProvider::Codex,
    MatrixFault::Checkpoint
);
live_fault_case!(
    codex_migration_publish,
    CODEX_MIGRATION_PUBLISH_CELL,
    AgentProvider::Codex,
    MatrixFault::MigrationPublish
);

live_fault_case!(
    claude_admission_write,
    CLAUDE_ADMISSION_WRITE_CELL,
    AgentProvider::Claude,
    MatrixFault::AdmissionWrite
);
live_fault_case!(
    claude_inference_exit,
    CLAUDE_INFERENCE_EXIT_CELL,
    AgentProvider::Claude,
    MatrixFault::InferenceExit
);
live_fault_case!(
    claude_commit_before_call,
    CLAUDE_COMMIT_BEFORE_CALL_CELL,
    AgentProvider::Claude,
    MatrixFault::CommitBeforeCall
);
live_fault_case!(
    claude_commit_after_return,
    CLAUDE_COMMIT_AFTER_RETURN_CELL,
    AgentProvider::Claude,
    MatrixFault::CommitAfterReturn
);
live_fault_case!(
    claude_stdout_write,
    CLAUDE_STDOUT_WRITE_CELL,
    AgentProvider::Claude,
    MatrixFault::StdoutWrite
);
live_fault_case!(
    claude_delivery_write,
    CLAUDE_DELIVERY_WRITE_CELL,
    AgentProvider::Claude,
    MatrixFault::DeliveryWrite
);
live_fault_case!(
    claude_checkpoint,
    CLAUDE_CHECKPOINT_CELL,
    AgentProvider::Claude,
    MatrixFault::Checkpoint
);
live_fault_case!(
    claude_migration_publish,
    CLAUDE_MIGRATION_PUBLISH_CELL,
    AgentProvider::Claude,
    MatrixFault::MigrationPublish
);

live_fault_case!(
    antigravity_admission_write,
    ANTIGRAVITY_ADMISSION_WRITE_CELL,
    AgentProvider::Antigravity,
    MatrixFault::AdmissionWrite
);
live_fault_case!(
    antigravity_inference_exit,
    ANTIGRAVITY_INFERENCE_EXIT_CELL,
    AgentProvider::Antigravity,
    MatrixFault::InferenceExit
);
live_fault_case!(
    antigravity_commit_before_call,
    ANTIGRAVITY_COMMIT_BEFORE_CALL_CELL,
    AgentProvider::Antigravity,
    MatrixFault::CommitBeforeCall
);
live_fault_case!(
    antigravity_commit_after_return,
    ANTIGRAVITY_COMMIT_AFTER_RETURN_CELL,
    AgentProvider::Antigravity,
    MatrixFault::CommitAfterReturn
);
live_fault_case!(
    antigravity_stdout_write,
    ANTIGRAVITY_STDOUT_WRITE_CELL,
    AgentProvider::Antigravity,
    MatrixFault::StdoutWrite
);
live_fault_case!(
    antigravity_delivery_write,
    ANTIGRAVITY_DELIVERY_WRITE_CELL,
    AgentProvider::Antigravity,
    MatrixFault::DeliveryWrite
);
live_fault_case!(
    antigravity_checkpoint,
    ANTIGRAVITY_CHECKPOINT_CELL,
    AgentProvider::Antigravity,
    MatrixFault::Checkpoint
);
live_fault_case!(
    antigravity_migration_publish,
    ANTIGRAVITY_MIGRATION_PUBLISH_CELL,
    AgentProvider::Antigravity,
    MatrixFault::MigrationPublish
);

const DECLARED_CELLS: [(AgentProvider, MatrixFault); 24] = [
    CODEX_ADMISSION_WRITE_CELL,
    CODEX_INFERENCE_EXIT_CELL,
    CODEX_COMMIT_BEFORE_CALL_CELL,
    CODEX_COMMIT_AFTER_RETURN_CELL,
    CODEX_STDOUT_WRITE_CELL,
    CODEX_DELIVERY_WRITE_CELL,
    CODEX_CHECKPOINT_CELL,
    CODEX_MIGRATION_PUBLISH_CELL,
    CLAUDE_ADMISSION_WRITE_CELL,
    CLAUDE_INFERENCE_EXIT_CELL,
    CLAUDE_COMMIT_BEFORE_CALL_CELL,
    CLAUDE_COMMIT_AFTER_RETURN_CELL,
    CLAUDE_STDOUT_WRITE_CELL,
    CLAUDE_DELIVERY_WRITE_CELL,
    CLAUDE_CHECKPOINT_CELL,
    CLAUDE_MIGRATION_PUBLISH_CELL,
    ANTIGRAVITY_ADMISSION_WRITE_CELL,
    ANTIGRAVITY_INFERENCE_EXIT_CELL,
    ANTIGRAVITY_COMMIT_BEFORE_CALL_CELL,
    ANTIGRAVITY_COMMIT_AFTER_RETURN_CELL,
    ANTIGRAVITY_STDOUT_WRITE_CELL,
    ANTIGRAVITY_DELIVERY_WRITE_CELL,
    ANTIGRAVITY_CHECKPOINT_CELL,
    ANTIGRAVITY_MIGRATION_PUBLISH_CELL,
];

fn assert_hung_child_without_marker_is_bounded_and_reaped() {
    let mut harness = FaultHarness::new(MatrixFault::AdmissionWrite);
    let writer = harness.marker_writer.as_ref().unwrap();
    let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags, -1);
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );
    let child = Command::new("/bin/sh")
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    harness.child = Some(child);
    harness.marker_writer.take();

    let started = Instant::now();
    let error = harness
        .finish_armed_with_deadlines(
            MatrixFault::AdmissionWrite,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        error.starts_with("marker deadline expired for AdmissionWrite; child output:"),
        "{error}"
    );
    assert!(harness.child.is_none());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

fn assert_hung_restart_is_bounded_and_reaped() {
    let mut harness = FaultHarness::new(MatrixFault::AdmissionWrite);
    let child = Command::new("/bin/sh")
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    harness.child = Some(child);

    let started = Instant::now();
    let error = harness
        .finish_unarmed_with_deadline(Duration::from_millis(50))
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(error, "restart process exit deadline expired");
    assert!(harness.child.is_none());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn runtime_cache_commit_faults_preserve_exact_brain_evidence() {
    let _guard = LIVE_FAULT_MATRIX_LOCK.lock().unwrap();
    for fault in [
        MatrixFault::CacheCommitBeforeCall,
        MatrixFault::CacheCommitAfterReturn,
    ] {
        let mut harness = FaultHarness::new(fault);
        harness.prepare_current_storage();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&harness.home)
                .status()
                .unwrap()
                .success()
        );
        let manifest = harness.home.join(".coding-brain/project.toml");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(
            manifest,
            "schema_version = 1\nproject_id = \"123e4567-e89b-12d3-a456-426614174000\"\n",
        )
        .unwrap();

        harness.spawn_armed_lifecycle(fault);
        let result = harness.finish_armed(fault);

        assert_eq!(result.status.signal(), Some(libc::SIGABRT));
        assert!(result.stdout.is_empty());
        let database = BrainDb::open_current(
            &StoragePaths::at(&harness.state_root),
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(2)),
        )
        .unwrap();
        let lifecycle = database.read_lifecycle().unwrap();
        let session = lifecycle
            .sessions
            .get(
                &AgentSessionKey::native(AgentProvider::Codex, "cache-fault-session").storage_key(),
            )
            .unwrap();
        assert_eq!(
            session.latest_event,
            Some(LifecycleEventName::UserPromptSubmit)
        );
        assert_eq!(session.cwd, harness.home);
        assert_eq!(
            session.transcript_path.as_deref(),
            Some(Path::new("/tmp/cache-fault-rollout.jsonl"))
        );
        assert_eq!(session.current_turn.as_deref(), Some("cache-fault-turn"));
        assert_eq!(
            session
                .last_signature
                .as_ref()
                .map(|signature| &signature.kind),
            Some(&LifecycleEventKind::UserPromptSubmit)
        );
        let activities = database
            .read_activity_page(None, 8, 128 * 1024)
            .unwrap()
            .events;
        assert_eq!(activities.len(), 1);
        let activity = &activities[0].event;
        assert_eq!(activity.kind, ActivityKind::Lifecycle);
        assert_eq!(activity.state, ActivityState::Abstained);
        assert_eq!(activity.tool.as_deref(), Some("UserPromptSubmit"));
        assert_eq!(
            activity.project.project_id,
            ProjectId::Stable("123e4567-e89b-12d3-a456-426614174000".into())
        );
        assert_eq!(activity.project.cwd, harness.home);
        let target = activity.session.as_ref().unwrap();
        assert_eq!(target.provider, AgentProvider::Codex);
        assert_eq!(target.session_id, "cache-fault-session");
        assert_eq!(target.turn_id.as_deref(), Some("cache-fault-turn"));
        assert_eq!(target.tool_use_id, None);
        assert_eq!(target.project_id, activity.project.project_id);
        assert_eq!(target.cwd, harness.home);
        assert_eq!(target.provenance, SessionTargetProvenance::Structured);
        let cache = harness.state_root.join("db/runtime-cache-v1.sqlite3");
        let rows = Connection::open(cache)
            .unwrap()
            .query_row("SELECT count(*) FROM project_identity_cache", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(
            rows,
            if fault == MatrixFault::CacheCommitAfterReturn {
                1
            } else {
                0
            }
        );
    }
}

#[test]
fn declared_matrix_is_the_exact_cartesian_product() {
    let _guard = LIVE_FAULT_MATRIX_LOCK.lock().unwrap();
    assert_hung_child_without_marker_is_bounded_and_reaped();
    assert_hung_restart_is_bounded_and_reaped();
    let declared = DECLARED_CELLS.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(
        declared.len(),
        DECLARED_CELLS.len(),
        "duplicate matrix pair"
    );
    assert_eq!(declared, expected_cells());
}
