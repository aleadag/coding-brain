use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::provider_hooks::{ShellCommandInput, ShellDialect};

mod shell;

#[cfg(unix)]
const HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(any(unix, test))]
const MAX_HELPER_OUTPUT_BYTES: usize = 512;
const MAX_SHELL_COMMAND_BYTES: usize = 64 * 1024;
const MAX_RECURSIVE_PARSE_BYTES: usize = MAX_SHELL_COMMAND_BYTES;
const MAX_NESTED_EXECUTION_DEPTH: usize = 8;
#[cfg(unix)]
const MAX_TRUSTED_HOME_BYTES: usize = 4_096;
const MAX_PATTERN_MATCH_STATES: usize = 16_384;
const MAX_PATTERN_MATCH_COMPONENTS: usize = 65_536;
const STARTUP_ENV_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_STARTUP_ENV_UNCERTAIN";
const LASTPIPE_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_LASTPIPE_ENABLED";
const POSIX_MODE_ENABLED_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_ENABLED";
const POSIX_MODE_UNCERTAIN_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_UNCERTAIN";
const POSIX_MODE_PROPAGATES_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_PROPAGATES";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PosixMode {
    Disabled,
    Enabled,
    Unknown,
}

#[derive(Clone, Copy)]
struct InheritedShellState {
    startup_environment_uncertain: bool,
    lastpipe_enabled: bool,
    posix_mode: PosixMode,
    posix_mode_propagates: bool,
}

impl InheritedShellState {
    fn from_parent_environment() -> Self {
        let bashopts = std::env::var_os("BASHOPTS");
        let lastpipe_enabled = bashopts.is_some_and(|bashopts| {
            bashopts
                .to_str()
                .is_none_or(|bashopts| bashopts.split(':').any(|option| option == "lastpipe"))
        });
        let posixly_correct = std::env::var_os("POSIXLY_CORRECT");
        let shellopts = std::env::var_os("SHELLOPTS");
        let posix_mode_propagates = posixly_correct.is_some() || shellopts.is_some();
        let posix_mode = if posixly_correct.is_some() {
            PosixMode::Enabled
        } else {
            match shellopts {
                None => PosixMode::Disabled,
                Some(shellopts) => match shellopts.to_str() {
                    Some(shellopts) if shellopts.split(':').any(|option| option == "posix") => {
                        PosixMode::Enabled
                    }
                    Some(_) => PosixMode::Disabled,
                    None => PosixMode::Unknown,
                },
            }
        };
        Self {
            startup_environment_uncertain: std::env::var_os("BASH_ENV").is_some()
                || std::env::var_os("ENV").is_some(),
            lastpipe_enabled,
            posix_mode,
            posix_mode_propagates,
        }
    }

    fn from_helper_environment() -> Self {
        let parent = Self::from_parent_environment();
        let posix_mode = if std::env::var_os(POSIX_MODE_UNCERTAIN_MARKER)
            .is_some_and(|value| value == "1")
        {
            PosixMode::Unknown
        } else if std::env::var_os(POSIX_MODE_ENABLED_MARKER).is_some_and(|value| value == "1") {
            PosixMode::Enabled
        } else {
            parent.posix_mode
        };
        Self {
            startup_environment_uncertain: parent.startup_environment_uncertain
                || std::env::var_os(STARTUP_ENV_MARKER).is_some_and(|value| value == "1"),
            lastpipe_enabled: parent.lastpipe_enabled
                || std::env::var_os(LASTPIPE_MARKER).is_some_and(|value| value == "1"),
            posix_mode,
            posix_mode_propagates: parent.posix_mode_propagates
                || std::env::var_os(POSIX_MODE_PROPAGATES_MARKER).is_some_and(|value| value == "1"),
        }
    }
}

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

#[derive(Clone)]
struct EvaluationState {
    trusted_home: Option<String>,
    assignments: HashMap<String, String>,
    ifs_unknown: bool,
    lastpipe_may_be_enabled: bool,
    posix_mode: PosixMode,
    posix_mode_propagates: bool,
    posix_child_mode: PosixMode,
    current_program_startup_may_be_unsafe: bool,
    child_environment_may_be_unsafe: bool,
    mutation_version: u64,
    assignment_mutations: HashMap<String, u64>,
}

impl EvaluationState {
    fn trusted() -> Self {
        Self::trusted_with_inherited(InheritedShellState::from_helper_environment())
    }

    fn trusted_with_inherited(inherited: InheritedShellState) -> Self {
        let mut assignments = HashMap::new();
        let trusted_home = std::env::var("HOME").ok();
        if let Some(home) = &trusted_home {
            assignments.insert("HOME".into(), home.clone());
        }
        Self {
            trusted_home,
            assignments,
            ifs_unknown: false,
            lastpipe_may_be_enabled: inherited.lastpipe_enabled,
            posix_mode: inherited.posix_mode,
            posix_mode_propagates: inherited.posix_mode_propagates,
            posix_child_mode: if inherited.posix_mode_propagates {
                inherited.posix_mode
            } else {
                PosixMode::Disabled
            },
            current_program_startup_may_be_unsafe: inherited.startup_environment_uncertain,
            child_environment_may_be_unsafe: inherited.startup_environment_uncertain,
            mutation_version: 0,
            assignment_mutations: HashMap::new(),
        }
    }

    fn child(
        &self,
        preserve_trusted_home: bool,
        startup_may_be_unsafe: bool,
        semantics: ShellSemantics,
    ) -> Self {
        let mut assignments = HashMap::new();
        if preserve_trusted_home
            && self
                .trusted_home
                .as_ref()
                .is_some_and(|home| self.assignments.get("HOME") == Some(home))
        {
            let home = self
                .trusted_home
                .as_ref()
                .expect("checked trusted HOME presence");
            assignments.insert("HOME".into(), home.clone());
        }
        Self {
            trusted_home: self.trusted_home.clone(),
            assignments,
            ifs_unknown: false,
            lastpipe_may_be_enabled: self.lastpipe_may_be_enabled,
            posix_mode: match semantics {
                ShellSemantics::Bash
                    if preserve_trusted_home
                        && !startup_may_be_unsafe
                        && self.posix_mode_propagates =>
                {
                    self.posix_child_mode
                }
                _ => semantics.initial_posix_mode(),
            },
            posix_mode_propagates: preserve_trusted_home
                && !startup_may_be_unsafe
                && self.posix_mode_propagates,
            posix_child_mode: if preserve_trusted_home
                && !startup_may_be_unsafe
                && self.posix_mode_propagates
            {
                self.posix_child_mode
            } else {
                PosixMode::Disabled
            },
            current_program_startup_may_be_unsafe: startup_may_be_unsafe,
            child_environment_may_be_unsafe: startup_may_be_unsafe,
            mutation_version: 0,
            assignment_mutations: HashMap::new(),
        }
    }

    fn next_mutation(&mut self) -> u64 {
        self.mutation_version = self.mutation_version.saturating_add(1);
        self.mutation_version
    }

    fn mark_assignment_mutation(&mut self, name: &str) {
        let version = self.next_mutation();
        self.assignment_mutations.insert(name.to_string(), version);
    }

    fn assignment_mutated_since(&self, name: &str, version: u64) -> bool {
        self.assignment_mutations
            .get(name)
            .is_some_and(|mutation| *mutation > version)
    }

    fn invalidate_mutable(&mut self) {
        self.assignments.clear();
        self.ifs_unknown = true;
        self.child_environment_may_be_unsafe = true;
    }

    fn note_environment_assignment(&mut self, name: &str) {
        if matches!(name, "BASH_ENV" | "ENV" | "POSIXLY_CORRECT" | "SHELLOPTS") {
            self.child_environment_may_be_unsafe = true;
        }
    }
}

struct TemporaryAssignment {
    name: String,
    original_value: Option<String>,
    original_ifs_unknown: bool,
}

struct AppliedEvalAssignments {
    temporary: Vec<TemporaryAssignment>,
    mutation_version: u64,
    indeterminate: bool,
}

impl AppliedEvalAssignments {
    fn restore(self, state: &mut EvaluationState) {
        for assignment in self.temporary {
            if state.assignment_mutated_since(&assignment.name, self.mutation_version) {
                state.assignments.remove(&assignment.name);
                if assignment.name == "IFS" {
                    state.ifs_unknown = true;
                }
                continue;
            }

            match assignment.original_value {
                Some(value) => {
                    state.assignments.insert(assignment.name.clone(), value);
                }
                None => {
                    state.assignments.remove(&assignment.name);
                }
            }
            if assignment.name == "IFS" {
                state.ifs_unknown = assignment.original_ifs_unknown;
            }
        }
    }
}

fn apply_eval_assignments(
    assignments: &[shell::ShellAssignment],
    state: &mut EvaluationState,
) -> AppliedEvalAssignments {
    let mut temporary = Vec::new();
    let mut indeterminate = false;

    for assignment in assignments {
        if !temporary
            .iter()
            .any(|saved: &TemporaryAssignment| saved.name == assignment.name)
        {
            temporary.push(TemporaryAssignment {
                name: assignment.name.clone(),
                original_value: state.assignments.get(&assignment.name).cloned(),
                original_ifs_unknown: state.ifs_unknown,
            });
        }

        let value = if assignment.value.may_mutate_shell_state {
            state.invalidate_mutable();
            indeterminate = true;
            None
        } else {
            resolve_word(&assignment.value, &state.assignments)
        };
        let value = if assignment.append {
            state
                .assignments
                .get(&assignment.name)
                .cloned()
                .zip(value)
                .map(|(mut current, value)| {
                    current.push_str(&value);
                    current
                })
        } else {
            value
        };

        match value {
            Some(value) => {
                state.assignments.insert(assignment.name.clone(), value);
                if assignment.name == "IFS" {
                    state.ifs_unknown = false;
                }
            }
            None => {
                state.assignments.remove(&assignment.name);
                if assignment.name == "IFS" {
                    state.ifs_unknown = true;
                }
                indeterminate = true;
            }
        }
        state.note_environment_assignment(&assignment.name);
        state.mark_assignment_mutation(&assignment.name);
    }

    AppliedEvalAssignments {
        temporary,
        mutation_version: state.mutation_version,
        indeterminate,
    }
}

struct EvaluationBudget {
    remaining_bytes: usize,
    remaining_nested: usize,
    shell: shell::AnalysisBudget,
    patterns: PatternMatchBudget,
}

impl EvaluationBudget {
    fn new() -> Self {
        Self::with_limits(
            MAX_RECURSIVE_PARSE_BYTES,
            MAX_NESTED_EXECUTION_DEPTH,
            shell::AnalysisBudget::default(),
        )
    }

    fn with_limits(
        remaining_bytes: usize,
        remaining_nested: usize,
        shell: shell::AnalysisBudget,
    ) -> Self {
        Self {
            remaining_bytes,
            remaining_nested,
            shell,
            patterns: PatternMatchBudget {
                remaining_states: MAX_PATTERN_MATCH_STATES,
                remaining_components: MAX_PATTERN_MATCH_COMPONENTS,
            },
        }
    }

    fn charge_parse(&mut self, source: &str) -> Result<(), ShellAnalysisError> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(source.len())
            .ok_or(ShellAnalysisError::ResourceLimit)?;
        Ok(())
    }

    fn evaluate_nested(
        &mut self,
        source: &str,
        state: &mut EvaluationState,
        semantics: ShellSemantics,
    ) -> SafetyEvaluation {
        let Some(remaining) = self.remaining_nested.checked_sub(1) else {
            return SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit);
        };
        self.remaining_nested = remaining;
        let result = evaluate_program(source, state, self, semantics);
        self.remaining_nested += 1;
        result
    }
}

#[derive(Default)]
struct EvaluationSummary {
    indeterminate: Option<ShellAnalysisError>,
}

impl EvaluationSummary {
    fn mark_indeterminate(&mut self, error: ShellAnalysisError) {
        self.indeterminate.get_or_insert(error);
    }

    fn observe(&mut self, result: SafetyEvaluation) -> Option<SafetyEvaluation> {
        match result {
            SafetyEvaluation::Deny(deny) => Some(SafetyEvaluation::Deny(deny)),
            SafetyEvaluation::Indeterminate(error) => {
                self.indeterminate.get_or_insert(error);
                None
            }
            SafetyEvaluation::NoDeterministicDecision => None,
        }
    }

    fn finish(self) -> SafetyEvaluation {
        self.indeterminate.map_or(
            SafetyEvaluation::NoDeterministicDecision,
            SafetyEvaluation::Indeterminate,
        )
    }
}

enum NestedExecution {
    CurrentShellInert {
        prefix_assignments_persist: bool,
    },
    ExternalSource {
        context: EvalContext,
        prefix_assignments_persist: bool,
    },
    DeferredLiteral {
        program: String,
        context: EvalContext,
        semantics: ShellSemantics,
        prefix_assignments_visible: bool,
        prefix_assignments_persist: bool,
    },
    DeferredUnresolved {
        error: ShellAnalysisError,
        prefix_assignments_persist: bool,
    },
    Eval {
        program: String,
        context: EvalContext,
        prefix_assignments_persist: bool,
    },
    EvalUnresolved(ShellAnalysisError),
    UnsafeExpansion,
    ChildLiteral {
        program: String,
        semantics: ShellSemantics,
        indeterminate_after_scan: bool,
        preserve_trusted_home: bool,
    },
    ChildUnresolved(ShellAnalysisError),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvalContext {
    Caller,
    Child,
    External,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellSemantics {
    Bash,
    BashPosix,
    Portable,
}

impl ShellSemantics {
    fn initial_posix_mode(self) -> PosixMode {
        match self {
            Self::Bash => PosixMode::Disabled,
            Self::BashPosix | Self::Portable => PosixMode::Enabled,
        }
    }

    fn eval_prefix_assignments_persist(self, posix_mode: PosixMode) -> Option<bool> {
        match self {
            Self::Portable => Some(true),
            Self::Bash | Self::BashPosix => match posix_mode {
                PosixMode::Disabled => Some(false),
                PosixMode::Enabled => Some(true),
                PosixMode::Unknown => None,
            },
        }
    }

    fn supports_builtin_dispatch(self) -> bool {
        self != Self::Portable
    }

    fn supports_time_keyword(self) -> bool {
        self != Self::Portable
    }
}

struct ClassifiedExecution<'a> {
    words: &'a [&'a shell::ShellWord],
    nested: Option<NestedExecution>,
    indeterminate_after_scan: bool,
    context: EvalContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafetyDeny {
    pub rule_id: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SafetyEvaluation {
    Deny(SafetyDeny),
    NoDeterministicDecision,
    Indeterminate(ShellAnalysisError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // The parser-isolation tasks consume every explicit analysis failure.
pub(crate) enum ShellAnalysisError {
    UnsupportedDialect,
    UnsupportedSyntax,
    ResourceLimit,
    HelperFailure,
}

const _: fn(&str) -> Result<shell::ShellProgram, ShellAnalysisError> = shell::analyze;

impl ShellAnalysisError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::UnsupportedDialect => "unsupported-shell-dialect",
            Self::UnsupportedSyntax => "unsupported-shell-syntax",
            Self::ResourceLimit => "shell-analysis-resource-limit",
            Self::HelperFailure => "shell-analysis-helper-failure",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum HelperResponse {
    Deny { rule_id: String },
    NoDeterministicDecision,
    Indeterminate,
}

fn deny_for_rule_id(rule_id: &str) -> Option<SafetyDeny> {
    match rule_id {
        "irreversible-root-delete" => Some(SafetyDeny {
            rule_id: "irreversible-root-delete",
            reason: "refusing recursive deletion of the filesystem root".into(),
        }),
        "irreversible-home-delete" => Some(SafetyDeny {
            rule_id: "irreversible-home-delete",
            reason: "refusing recursive deletion of the home directory".into(),
        }),
        "unsafe-recursive-delete-expansion" => Some(SafetyDeny {
            rule_id: "unsafe-recursive-delete-expansion",
            reason: "refusing execution through unresolved or dynamically parsed shell input"
                .into(),
        }),
        _ => None,
    }
}

#[cfg(any(unix, test))]
fn decode_helper_response(output: &[u8]) -> SafetyEvaluation {
    let response = serde_json::from_slice::<HelperResponse>(output)
        .ok()
        .zip(serde_json::from_slice::<serde_json::Value>(output).ok())
        .and_then(|(response, value)| {
            let object = value.as_object()?;
            let expected_fields = match object.get("result")?.as_str()? {
                "deny" => 2,
                "no_deterministic_decision" | "indeterminate" => 1,
                _ => return None,
            };
            if object.len() != expected_fields {
                return None;
            }
            Some(response)
        });
    match response {
        Some(HelperResponse::Deny { rule_id }) => deny_for_rule_id(&rule_id)
            .map(SafetyEvaluation::Deny)
            .unwrap_or(SafetyEvaluation::Indeterminate(
                ShellAnalysisError::HelperFailure,
            )),
        Some(HelperResponse::NoDeterministicDecision) => SafetyEvaluation::NoDeterministicDecision,
        Some(HelperResponse::Indeterminate) | None => {
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        }
    }
}

#[cfg(unix)]
fn evaluate_isolated_with(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> SafetyEvaluation {
    crate::provider_hooks::run_bounded_process(command, timeout, MAX_HELPER_OUTPUT_BYTES).map_or(
        SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure),
        |output| decode_helper_response(&output),
    )
}

#[cfg(unix)]
fn validate_trusted_home_context(home: Option<OsString>) -> Result<OsString, ShellAnalysisError> {
    use std::os::unix::ffi::OsStrExt;

    let home = home.ok_or(ShellAnalysisError::HelperFailure)?;
    if home.as_os_str().is_empty()
        || !Path::new(&home).is_absolute()
        || home.as_os_str().as_bytes().len() > MAX_TRUSTED_HOME_BYTES
        || home.to_str().is_none()
    {
        return Err(ShellAnalysisError::HelperFailure);
    }
    Ok(home)
}

#[cfg(unix)]
fn evaluate_isolated_with_home(
    input: &ShellCommandInput,
    home: Option<OsString>,
    run_helper: impl FnOnce(&ShellCommandInput, OsString) -> SafetyEvaluation,
) -> SafetyEvaluation {
    match validate_trusted_home_context(home) {
        Ok(home) => run_helper(input, home),
        Err(error) => SafetyEvaluation::Indeterminate(error),
    }
}

#[cfg(unix)]
fn configure_isolated_helper_environment(
    command: &mut std::process::Command,
    trusted_home: OsString,
    inherited: InheritedShellState,
) {
    command
        .env_clear()
        .env("HOME", trusted_home)
        .env(
            STARTUP_ENV_MARKER,
            if inherited.startup_environment_uncertain {
                "1"
            } else {
                "0"
            },
        )
        .env(
            LASTPIPE_MARKER,
            if inherited.lastpipe_enabled { "1" } else { "0" },
        )
        .env(
            POSIX_MODE_ENABLED_MARKER,
            if inherited.posix_mode == PosixMode::Enabled {
                "1"
            } else {
                "0"
            },
        )
        .env(
            POSIX_MODE_UNCERTAIN_MARKER,
            if inherited.posix_mode == PosixMode::Unknown {
                "1"
            } else {
                "0"
            },
        )
        .env(
            POSIX_MODE_PROPAGATES_MARKER,
            if inherited.posix_mode_propagates {
                "1"
            } else {
                "0"
            },
        );
}

pub(crate) fn evaluate_isolated(command: Option<&ShellCommandInput>) -> SafetyEvaluation {
    let Some(input) = command else {
        return SafetyEvaluation::NoDeterministicDecision;
    };
    if input.dialect != ShellDialect::Bash {
        return SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedDialect);
    }
    if input.source.len() > MAX_SHELL_COMMAND_BYTES {
        return SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure);
    }

    #[cfg(unix)]
    {
        use std::io::{Seek, Write};
        use std::process::{Command, Stdio};

        let inherited = InheritedShellState::from_parent_environment();
        evaluate_isolated_with_home(input, std::env::var_os("HOME"), |input, trusted_home| {
            let mut helper_input = match tempfile::tempfile() {
                Ok(input) => input,
                Err(_) => {
                    return SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure);
                }
            };
            if helper_input.write_all(input.source.as_bytes()).is_err()
                || helper_input.rewind().is_err()
            {
                return SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure);
            }
            let executable = match std::env::current_exe() {
                Ok(executable) => executable,
                Err(_) => {
                    return SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure);
                }
            };
            let mut command = Command::new(executable);
            command
                .arg("--shell-safety-helper")
                .stdin(Stdio::from(helper_input));
            configure_isolated_helper_environment(&mut command, trusted_home, inherited);
            evaluate_isolated_with(&mut command, HELPER_TIMEOUT)
        })
    }

    #[cfg(not(unix))]
    {
        SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
    }
}

fn helper_response(evaluation: SafetyEvaluation) -> HelperResponse {
    match evaluation {
        SafetyEvaluation::Deny(deny) => HelperResponse::Deny {
            rule_id: deny.rule_id.into(),
        },
        SafetyEvaluation::NoDeterministicDecision => HelperResponse::NoDeterministicDecision,
        SafetyEvaluation::Indeterminate(_) => HelperResponse::Indeterminate,
    }
}

fn run_helper_with(reader: impl std::io::Read, writer: impl std::io::Write) -> std::io::Result<()> {
    run_helper_with_optional_inherited(reader, writer, None)
}

#[cfg(test)]
fn run_helper_with_inherited(
    reader: impl std::io::Read,
    writer: impl std::io::Write,
    inherited: InheritedShellState,
) -> std::io::Result<()> {
    run_helper_with_optional_inherited(reader, writer, Some(inherited))
}

fn run_helper_with_optional_inherited(
    mut reader: impl std::io::Read,
    mut writer: impl std::io::Write,
    inherited: Option<InheritedShellState>,
) -> std::io::Result<()> {
    use std::io::Read as _;

    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_SHELL_COMMAND_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_SHELL_COMMAND_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "shell safety helper input exceeds limit",
        ));
    }
    let source = String::from_utf8(input)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8"))?;
    let input = ShellCommandInput {
        dialect: ShellDialect::Bash,
        source,
    };
    let evaluation = inherited.map_or_else(
        || evaluate_in_process(Some(&input)),
        |inherited| evaluate_in_process_with_inherited(Some(&input), inherited),
    );
    serde_json::to_writer(&mut writer, &helper_response(evaluation))
        .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

#[allow(dead_code)] // The library target has no CLI; the binary dispatches this hidden helper.
pub(crate) fn run_helper() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_helper_with(stdin.lock(), stdout.lock())
}

#[allow(dead_code)] // The binary-only helper uses this through its duplicated module tree.
pub(crate) fn evaluate_in_process(command: Option<&ShellCommandInput>) -> SafetyEvaluation {
    evaluate_in_process_with_state(command, EvaluationState::trusted())
}

fn evaluate_in_process_with_inherited(
    command: Option<&ShellCommandInput>,
    inherited: InheritedShellState,
) -> SafetyEvaluation {
    evaluate_in_process_with_state(command, EvaluationState::trusted_with_inherited(inherited))
}

#[cfg(test)]
pub(super) fn evaluate_in_process_with_inherited_startup(
    command: Option<&ShellCommandInput>,
) -> SafetyEvaluation {
    evaluate_in_process_with_inherited(
        command,
        InheritedShellState {
            startup_environment_uncertain: true,
            lastpipe_enabled: false,
            posix_mode: PosixMode::Disabled,
            posix_mode_propagates: false,
        },
    )
}

#[cfg(test)]
pub(super) fn evaluate_in_process_with_inherited_posix(
    command: Option<&ShellCommandInput>,
) -> SafetyEvaluation {
    evaluate_in_process_with_inherited(
        command,
        InheritedShellState {
            startup_environment_uncertain: false,
            lastpipe_enabled: false,
            posix_mode: PosixMode::Enabled,
            posix_mode_propagates: true,
        },
    )
}

fn evaluate_in_process_with_state(
    command: Option<&ShellCommandInput>,
    mut state: EvaluationState,
) -> SafetyEvaluation {
    let Some(input) = command else {
        return SafetyEvaluation::NoDeterministicDecision;
    };
    if input.dialect != ShellDialect::Bash {
        return SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedDialect);
    }

    evaluate_program(
        &input.source,
        &mut state,
        &mut EvaluationBudget::new(),
        ShellSemantics::Bash,
    )
}

struct EvalProgramResult {
    evaluation: SafetyEvaluation,
    invalidate_caller: bool,
}

fn evaluate_eval_program(
    program: &str,
    assignments: &[shell::ShellAssignment],
    state: &mut EvaluationState,
    budget: &mut EvaluationBudget,
    semantics: ShellSemantics,
    prefix_assignments_persist: bool,
) -> EvalProgramResult {
    let applied = apply_eval_assignments(assignments, state);
    let indeterminate_assignment = applied.indeterminate;
    let result = budget.evaluate_nested(program, state, semantics);
    let invalidate_caller = matches!(result, SafetyEvaluation::Indeterminate(_));
    if !prefix_assignments_persist {
        applied.restore(state);
    }

    let evaluation = match result {
        SafetyEvaluation::NoDeterministicDecision if indeterminate_assignment => {
            SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
        }
        result => result,
    };
    EvalProgramResult {
        evaluation,
        invalidate_caller,
    }
}

fn persist_prefix_assignments(
    assignments: &[shell::ShellAssignment],
    context: shell::ExecutionContext,
    state: &mut EvaluationState,
    persist: bool,
) {
    if !persist || assignments.is_empty() {
        return;
    }
    match context {
        shell::ExecutionContext::TopLevel => {
            let _applied = apply_eval_assignments(assignments, state);
        }
        shell::ExecutionContext::Conditional | shell::ExecutionContext::Loop => {
            state.invalidate_mutable();
        }
        shell::ExecutionContext::Pipeline if state.lastpipe_may_be_enabled => {
            state.invalidate_mutable();
        }
        shell::ExecutionContext::Asynchronous
        | shell::ExecutionContext::Pipeline
        | shell::ExecutionContext::Group
        | shell::ExecutionContext::Subshell
        | shell::ExecutionContext::ProcessSubstitution => {}
    }
}

fn evaluate_program(
    source: &str,
    state: &mut EvaluationState,
    budget: &mut EvaluationBudget,
    semantics: ShellSemantics,
) -> SafetyEvaluation {
    if let Err(error) = budget.charge_parse(source) {
        return SafetyEvaluation::Indeterminate(error);
    }
    let program = match shell::analyze_with_budget(source, &mut budget.shell) {
        Ok(program) => program,
        Err(error) => return SafetyEvaluation::Indeterminate(error),
    };
    if program.features.command_substitution
        || program.features.process_substitution
        || program.features.executable_group
    {
        return dynamic_execution_deny();
    }

    let mut summary = EvaluationSummary::default();
    if state.current_program_startup_may_be_unsafe {
        summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
    }
    for command in &program.commands {
        if matches!(
            command.context,
            shell::ExecutionContext::TopLevel
                | shell::ExecutionContext::Conditional
                | shell::ExecutionContext::Loop
        ) {
            for name in &command.invalidated_assignments {
                state.assignments.remove(name);
                if name == "IFS" {
                    state.ifs_unknown = true;
                }
                state.note_environment_assignment(name);
                state.mark_assignment_mutation(name);
            }
        }

        if command.timed_pipeline_head && !semantics.supports_time_keyword() {
            if matches!(
                command.context,
                shell::ExecutionContext::TopLevel
                    | shell::ExecutionContext::Conditional
                    | shell::ExecutionContext::Loop
            ) || (command.context == shell::ExecutionContext::Pipeline
                && state.lastpipe_may_be_enabled)
            {
                state.invalidate_mutable();
            }
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            continue;
        }

        let Some(command_word) = &command.command else {
            let may_mutate_shell_state = command
                .assignments
                .iter()
                .any(|assignment| assignment.value.may_mutate_shell_state);
            for assignment in &command.assignments {
                match command.context {
                    shell::ExecutionContext::TopLevel => {
                        let value_known = match assignment.value.literal.as_deref() {
                            Some(value) if assignment.append => {
                                if let Some(current) = state.assignments.get_mut(&assignment.name) {
                                    current.push_str(value);
                                    true
                                } else {
                                    false
                                }
                            }
                            Some(value) => {
                                state
                                    .assignments
                                    .insert(assignment.name.clone(), value.to_string());
                                true
                            }
                            None => false,
                        };
                        if value_known {
                            if assignment.name == "IFS" {
                                state.ifs_unknown = false;
                            }
                        } else {
                            state.assignments.remove(&assignment.name);
                            if assignment.name == "IFS" {
                                state.ifs_unknown = true;
                            }
                        }
                        state.mark_assignment_mutation(&assignment.name);
                        state.note_environment_assignment(&assignment.name);
                    }
                    shell::ExecutionContext::Conditional | shell::ExecutionContext::Loop => {
                        state.assignments.remove(&assignment.name);
                        if assignment.name == "IFS" {
                            state.ifs_unknown = true;
                        }
                        state.note_environment_assignment(&assignment.name);
                        state.mark_assignment_mutation(&assignment.name);
                    }
                    shell::ExecutionContext::Pipeline
                    | shell::ExecutionContext::Asynchronous
                    | shell::ExecutionContext::Group
                    | shell::ExecutionContext::Subshell
                    | shell::ExecutionContext::ProcessSubstitution => {}
                }
            }
            if (command.assignments.is_empty()
                && matches!(
                    command.context,
                    shell::ExecutionContext::TopLevel
                        | shell::ExecutionContext::Conditional
                        | shell::ExecutionContext::Loop
                ))
                || (may_mutate_shell_state
                    && matches!(
                        command.context,
                        shell::ExecutionContext::TopLevel
                            | shell::ExecutionContext::Conditional
                            | shell::ExecutionContext::Loop
                    ))
                || (command.context == shell::ExecutionContext::Pipeline
                    && state.lastpipe_may_be_enabled)
            {
                state.invalidate_mutable();
            }
            continue;
        };

        let mut words = Vec::with_capacity(command.arguments.len() + 1);
        words.push(command_word);
        words.extend(&command.arguments);
        let eval_prefix_assignments_persist =
            semantics.eval_prefix_assignments_persist(state.posix_mode);
        if eval_prefix_assignments_persist.is_none() {
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
        }
        let unwrapped = match unwrap_command(
            &words,
            command.assignments.is_empty() && command.redirects.is_empty(),
            eval_prefix_assignments_persist.unwrap_or(false),
        ) {
            Ok(unwrapped) => unwrapped,
            Err(UnwrapError::DynamicOrUnsupported | UnwrapError::UnsafeExpansion) => {
                return dynamic_execution_deny();
            }
        };
        let classified = classify_nested_execution(
            unwrapped.words,
            unwrapped.indeterminate_child_context || !command.assignments.is_empty(),
            unwrapped.indeterminate_after_scan,
            unwrapped.eval_context,
            unwrapped.time_keyword_allowed,
            unwrapped.eval_prefix_assignments_persist,
            semantics,
        );
        let wrapper_indeterminate_after_scan = classified.indeterminate_after_scan;
        let execution_context = classified.context;
        let words = classified.words;
        let command_name_literal = if classified.nested.is_none() {
            let Some(command_word) = words.first() else {
                if wrapper_indeterminate_after_scan {
                    summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
                }
                continue;
            };
            let Some(command_name_literal) = command_word.literal.as_deref() else {
                if word_can_select_command(command_word) {
                    return dynamic_execution_deny();
                }
                if wrapper_indeterminate_after_scan {
                    summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
                }
                continue;
            };
            Some(command_name_literal)
        } else {
            None
        };
        if let Some(nested) = classified.nested {
            let nested_result = match nested {
                NestedExecution::Eval {
                    program,
                    context: EvalContext::Caller,
                    prefix_assignments_persist,
                } => {
                    let result = if eval_prefix_assignments_persist.is_none() {
                        let mut nested_state = state.clone();
                        evaluate_eval_program(
                            &program,
                            &command.assignments,
                            &mut nested_state,
                            budget,
                            semantics,
                            prefix_assignments_persist,
                        )
                    } else {
                        match command.context {
                            shell::ExecutionContext::TopLevel => evaluate_eval_program(
                                &program,
                                &command.assignments,
                                state,
                                budget,
                                semantics,
                                prefix_assignments_persist,
                            ),
                            shell::ExecutionContext::Conditional
                            | shell::ExecutionContext::Loop => {
                                let mut nested_state = state.clone();
                                let result = evaluate_eval_program(
                                    &program,
                                    &command.assignments,
                                    &mut nested_state,
                                    budget,
                                    semantics,
                                    prefix_assignments_persist,
                                );
                                state.invalidate_mutable();
                                result
                            }
                            shell::ExecutionContext::Asynchronous
                            | shell::ExecutionContext::Pipeline
                            | shell::ExecutionContext::Group
                            | shell::ExecutionContext::Subshell
                            | shell::ExecutionContext::ProcessSubstitution => {
                                let mut nested_state = state.clone();
                                let result = evaluate_eval_program(
                                    &program,
                                    &command.assignments,
                                    &mut nested_state,
                                    budget,
                                    semantics,
                                    prefix_assignments_persist,
                                );
                                if command.context == shell::ExecutionContext::Pipeline
                                    && state.lastpipe_may_be_enabled
                                {
                                    state.invalidate_mutable();
                                }
                                result
                            }
                        }
                    };
                    if eval_prefix_assignments_persist.is_some()
                        && result.invalidate_caller
                        && command.context == shell::ExecutionContext::TopLevel
                    {
                        state.invalidate_mutable();
                    }
                    result.evaluation
                }
                NestedExecution::Eval {
                    program,
                    context: EvalContext::Child,
                    prefix_assignments_persist,
                } => {
                    let mut child_state = state.child(
                        false,
                        state.child_environment_may_be_unsafe,
                        ShellSemantics::Bash,
                    );
                    evaluate_eval_program(
                        &program,
                        &command.assignments,
                        &mut child_state,
                        budget,
                        ShellSemantics::Bash,
                        prefix_assignments_persist,
                    )
                    .evaluation
                }
                NestedExecution::Eval {
                    context: EvalContext::External,
                    ..
                } => unreachable!("external commands cannot dispatch shell eval"),
                NestedExecution::CurrentShellInert {
                    prefix_assignments_persist,
                } => {
                    persist_prefix_assignments(
                        &command.assignments,
                        command.context,
                        state,
                        prefix_assignments_persist,
                    );
                    SafetyEvaluation::NoDeterministicDecision
                }
                NestedExecution::ExternalSource {
                    context: EvalContext::Caller,
                    prefix_assignments_persist,
                } => {
                    persist_prefix_assignments(
                        &command.assignments,
                        command.context,
                        state,
                        prefix_assignments_persist,
                    );
                    if matches!(
                        command.context,
                        shell::ExecutionContext::TopLevel
                            | shell::ExecutionContext::Conditional
                            | shell::ExecutionContext::Loop
                    ) || (command.context == shell::ExecutionContext::Pipeline
                        && state.lastpipe_may_be_enabled)
                    {
                        state.invalidate_mutable();
                    }
                    SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
                }
                NestedExecution::ExternalSource {
                    context: EvalContext::Child,
                    ..
                } => SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax),
                NestedExecution::ExternalSource {
                    context: EvalContext::External,
                    ..
                } => unreachable!("external commands cannot dispatch shell source"),
                NestedExecution::DeferredLiteral {
                    program,
                    context,
                    semantics,
                    prefix_assignments_visible,
                    prefix_assignments_persist,
                } => {
                    let mut nested_state = match context {
                        EvalContext::Caller => state.clone(),
                        EvalContext::Child => {
                            state.child(false, state.child_environment_may_be_unsafe, semantics)
                        }
                        EvalContext::External => {
                            unreachable!("external commands cannot dispatch shell builtins")
                        }
                    };
                    if prefix_assignments_visible {
                        apply_eval_assignments(&command.assignments, &mut nested_state);
                    }
                    persist_prefix_assignments(
                        &command.assignments,
                        command.context,
                        state,
                        prefix_assignments_persist,
                    );
                    match budget.evaluate_nested(&program, &mut nested_state, semantics) {
                        SafetyEvaluation::NoDeterministicDecision => {
                            SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
                        }
                        result => result,
                    }
                }
                NestedExecution::DeferredUnresolved {
                    error,
                    prefix_assignments_persist,
                } => {
                    persist_prefix_assignments(
                        &command.assignments,
                        command.context,
                        state,
                        prefix_assignments_persist,
                    );
                    SafetyEvaluation::Indeterminate(error)
                }
                NestedExecution::EvalUnresolved(error) => {
                    if matches!(
                        command.context,
                        shell::ExecutionContext::TopLevel
                            | shell::ExecutionContext::Conditional
                            | shell::ExecutionContext::Loop
                    ) || (command.context == shell::ExecutionContext::Pipeline
                        && state.lastpipe_may_be_enabled)
                    {
                        state.invalidate_mutable();
                    }
                    SafetyEvaluation::Indeterminate(error)
                }
                NestedExecution::UnsafeExpansion => dynamic_execution_deny(),
                NestedExecution::ChildLiteral {
                    program,
                    semantics,
                    indeterminate_after_scan,
                    preserve_trusted_home,
                } => {
                    let mut child_state = state.child(
                        preserve_trusted_home,
                        state.child_environment_may_be_unsafe,
                        semantics,
                    );
                    if indeterminate_after_scan && semantics == ShellSemantics::Bash {
                        child_state.posix_mode = PosixMode::Unknown;
                        child_state.posix_child_mode = PosixMode::Unknown;
                    }
                    match budget.evaluate_nested(&program, &mut child_state, semantics) {
                        SafetyEvaluation::NoDeterministicDecision if indeterminate_after_scan => {
                            SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
                        }
                        result => result,
                    }
                }
                NestedExecution::ChildUnresolved(error) => SafetyEvaluation::Indeterminate(error),
            };
            let nested_result = match nested_result {
                SafetyEvaluation::NoDeterministicDecision if wrapper_indeterminate_after_scan => {
                    SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
                }
                result => result,
            };
            if let Some(deny) = summary.observe(nested_result) {
                return deny;
            }
            continue;
        }
        let command_name_literal =
            command_name_literal.expect("ordinary execution has a literal command name");

        update_lastpipe_state(
            command_name_literal,
            &words[1..],
            command.context,
            execution_context,
            state,
        );
        let child_environment_was_unsafe = state.child_environment_may_be_unsafe;
        let posix_only_set = update_posix_and_dispatch_state(
            command_name_literal,
            &words[1..],
            command.context,
            execution_context,
            state,
            &mut summary,
        );

        let command_assignments = state.assignments.clone();
        let command_ifs_unknown = state.ifs_unknown;
        if matches!(
            command.context,
            shell::ExecutionContext::TopLevel
                | shell::ExecutionContext::Conditional
                | shell::ExecutionContext::Loop
        ) || (command.context == shell::ExecutionContext::Pipeline
            && state.lastpipe_may_be_enabled)
        {
            state.invalidate_mutable();
            if posix_only_set && command.assignments.is_empty() {
                state.child_environment_may_be_unsafe = child_environment_was_unsafe;
            }
        }
        if command_name(command_name_literal) != "rm" {
            if wrapper_indeterminate_after_scan {
                summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            }
            continue;
        }
        let args = &words[1..];
        let mut recursive = false;
        let mut options_ended = false;
        for argument in args {
            if options_ended {
                continue;
            }
            match argument.literal.as_deref() {
                Some("--") => options_ended = true,
                Some(literal) if is_recursive_flag(literal) => recursive = true,
                Some(_) => {}
                None if word_can_supply_flag(argument)
                    && !word_is_definite_target(
                        argument,
                        &command_assignments,
                        command_ifs_unknown,
                        state.trusted_home.as_deref(),
                    ) =>
                {
                    return dynamic_execution_deny();
                }
                None => {}
            }
        }
        if !recursive {
            if wrapper_indeterminate_after_scan {
                summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            }
            continue;
        }

        options_ended = false;
        for target in args {
            if let Some(target) = target.literal.as_deref() {
                if !options_ended && target == "--" {
                    options_ended = true;
                    continue;
                }
                if !options_ended && target.starts_with('-') {
                    continue;
                }
                if is_root_target(target) {
                    return canonical_deny("irreversible-root-delete");
                }
                if literal_home_or_ancestor_target(target, state.trusted_home.as_deref()) {
                    return canonical_deny("irreversible-home-delete");
                }
                continue;
            }
            if word_is_home_target(target, &command_assignments, state.trusted_home.as_deref()) {
                return canonical_deny("irreversible-home-delete");
            }
            if resolve_word(target, &command_assignments).is_some_and(|resolved| {
                literal_home_or_ancestor_target(&resolved, state.trusted_home.as_deref())
            }) {
                return canonical_deny("irreversible-home-delete");
            }
            match split_target_risk(
                target,
                &command_assignments,
                command_ifs_unknown,
                state.trusted_home.as_deref(),
                &mut budget.patterns,
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
            if dynamic_target_is_dangerous(
                target,
                &command_assignments,
                command_ifs_unknown,
                state.trusted_home.as_deref(),
            ) {
                return expansion_target_deny();
            }
        }
        if wrapper_indeterminate_after_scan {
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
        }
    }
    summary.finish()
}

fn canonical_deny(rule_id: &str) -> SafetyEvaluation {
    SafetyEvaluation::Deny(
        deny_for_rule_id(rule_id).unwrap_or_else(|| unreachable!("canonical safety rule")),
    )
}

fn dynamic_execution_deny() -> SafetyEvaluation {
    canonical_deny("unsafe-recursive-delete-expansion")
}

fn expansion_target_deny() -> SafetyEvaluation {
    SafetyEvaluation::Deny(SafetyDeny {
        rule_id: "unsafe-recursive-delete-expansion",
        reason:
            "refusing recursive deletion through an unresolved, empty, or root-valued expansion"
                .into(),
    })
}

fn is_recursive_flag(argument: &str) -> bool {
    argument
        .strip_prefix("--")
        .is_some_and(|name| !name.is_empty() && "recursive".starts_with(name))
        || (argument.starts_with('-')
            && !argument.starts_with("--")
            && argument[1..].contains(['r', 'R']))
}

fn is_root_target(target: &str) -> bool {
    lexical_absolute_parts(Path::new(target)).is_some_and(|parts| parts.is_empty())
}

fn path_is_home_or_ancestor(target: &Path, home: &Path) -> bool {
    let Some(target) = lexical_absolute_parts(target) else {
        return false;
    };
    if target.is_empty() {
        return false;
    }
    lexical_absolute_parts(home)
        .is_some_and(|home| target.len() <= home.len() && home.starts_with(&target))
}

fn literal_home_or_ancestor_target(target: &str, trusted_home: Option<&str>) -> bool {
    trusted_home.is_some_and(|home| path_is_home_or_ancestor(Path::new(target), Path::new(home)))
}

fn literal_home_target(target: &str, trusted_home: Option<&str>) -> bool {
    let Some(target) = lexical_absolute_parts(Path::new(target)) else {
        return false;
    };
    trusted_home
        .and_then(|home| lexical_absolute_parts(Path::new(home)))
        .is_some_and(|home| target == home)
}

fn split_target_risk(
    target: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    ifs_unknown: bool,
    trusted_home: Option<&str>,
    budget: &mut PatternMatchBudget,
) -> SplitTargetRisk {
    if !target.can_split_fields {
        return SplitTargetRisk::None;
    }
    let Some(fields) = resolve_word_fields(target, assignments, ifs_unknown) else {
        return SplitTargetRisk::UnsafeExpansion;
    };
    let Some(home) = trusted_home.and_then(|home| lexical_absolute_parts(Path::new(home))) else {
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

        let path = Path::new(&field.value);
        if let Some(parts) = lexical_absolute_parts(path) {
            if !parts.is_empty() && parts.len() <= home.len() && home.starts_with(&parts) {
                return SplitTargetRisk::HomeOrAncestor;
            }
            continue;
        }

        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => parts.push(part.to_os_string()),
                Component::ParentDir => return SplitTargetRisk::UnsafeExpansion,
                Component::RootDir | Component::Prefix(_) => {
                    return SplitTargetRisk::UnsafeExpansion;
                }
            }
        }
        if !parts.is_empty()
            && (1..=home.len()).any(|ancestor_len| {
                let ancestor = &home[..ancestor_len];
                parts.len() <= ancestor.len() && ancestor.ends_with(&parts)
            })
        {
            return SplitTargetRisk::HomeOrAncestor;
        }
        for ancestor_len in 1..=home.len() {
            for start in 0..ancestor_len {
                match pattern_reachability(field, &home[start..ancestor_len], false, budget) {
                    PatternReachability::Reachable(_) => {
                        return SplitTargetRisk::HomeOrAncestor;
                    }
                    PatternReachability::Unknown => {
                        return SplitTargetRisk::UnsafeExpansion;
                    }
                    PatternReachability::Unreachable => {}
                }
            }
        }
    }

    SplitTargetRisk::None
}

fn pattern_reachability(
    field: &ResolvedField,
    parts: &[OsString],
    absolute: bool,
    budget: &mut PatternMatchBudget,
) -> PatternReachability {
    if !field.parameter_pathname_pattern {
        return PatternReachability::Unreachable;
    }
    let Some(candidate_parts) = parts
        .iter()
        .map(|part| part.to_str())
        .collect::<Option<Vec<_>>>()
    else {
        return PatternReachability::Unknown;
    };
    let Some((direct_absolute, direct_parts)) = direct_pattern_parts(&field.value) else {
        return PatternReachability::Unknown;
    };
    if direct_absolute == absolute
        && direct_pattern_components_may_match(&direct_parts, &candidate_parts)
    {
        return PatternReachability::Reachable(PatternMatchKind::DirectExpansion);
    }
    let Some((pattern_absolute, pattern_parts)) = pattern_parts(&field.value) else {
        return PatternReachability::Unknown;
    };
    if pattern_absolute != absolute {
        return PatternReachability::Unreachable;
    }
    pattern_components_may_normalize_to(&pattern_parts, &candidate_parts, absolute, budget)
}

fn direct_pattern_parts(pattern: &str) -> Option<(bool, Vec<String>)> {
    let mut absolute = false;
    let mut parts = Vec::new();
    for component in Path::new(pattern).components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::Prefix(_) => return None,
        }
    }
    Some((absolute, parts))
}

fn direct_pattern_components_may_match(patterns: &[String], candidates: &[&str]) -> bool {
    let Some(first_globstar) = patterns.iter().position(|part| part == "**") else {
        return patterns.len() == candidates.len()
            && patterns
                .iter()
                .zip(candidates)
                .all(|(pattern, candidate)| pattern_may_match_literal(pattern, candidate));
    };
    let last_globstar = patterns
        .iter()
        .rposition(|part| part == "**")
        .unwrap_or(first_globstar);
    let prefix = &patterns[..first_globstar];
    let suffix = &patterns[last_globstar + 1..];
    prefix.len() + suffix.len() <= candidates.len()
        && prefix
            .iter()
            .zip(candidates)
            .all(|(pattern, candidate)| pattern_may_match_literal(pattern, candidate))
        && suffix
            .iter()
            .rev()
            .zip(candidates.iter().rev())
            .all(|(pattern, candidate)| pattern_may_match_literal(pattern, candidate))
}

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
    if budget.remaining_states == 0 || budget.remaining_components < stored_components {
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
                && state
                    .resolved
                    .iter()
                    .zip(candidates)
                    .all(|(part, candidate)| match part {
                        ResolvedPatternComponent::Literal(index) => match &patterns[*index] {
                            PatternComponent::Literal(pattern) => {
                                pattern_may_match_literal(pattern, candidate)
                            }
                            _ => unreachable!("literal state points to a literal pattern"),
                        },
                        ResolvedPatternComponent::Any => true,
                    })
            {
                return PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal);
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
                    next.resolved
                        .extend(std::iter::repeat_n(ResolvedPatternComponent::Any, consumed));
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

fn lexical_absolute_parts(path: &Path) -> Option<Vec<OsString>> {
    let mut absolute = false;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::Prefix(_) => return None,
        }
    }
    absolute.then_some(parts)
}

fn dynamic_target_is_dangerous(
    target: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    ifs_unknown: bool,
    trusted_home: Option<&str>,
) -> bool {
    if target.can_split_fields {
        return resolve_word_fields(target, assignments, ifs_unknown).is_none_or(|fields| {
            fields.iter().any(|field| {
                is_root_target(&field.value)
                    || literal_home_target(&field.value, trusted_home)
                    || (!Path::new(&field.value).is_absolute()
                        && Path::new(&field.value)
                            .components()
                            .any(|component| component == Component::ParentDir))
            })
        });
    }
    if let Some(resolved) = resolve_word(target, assignments) {
        return resolved.is_empty() || is_root_target(&resolved);
    }
    target.parts.iter().any(word_part_is_target_dynamic)
}

fn resolve_word(word: &shell::ShellWord, assignments: &HashMap<String, String>) -> Option<String> {
    let mut resolved = String::new();
    for part in &word.parts {
        resolved.push_str(&resolve_word_part(part, word, assignments)?);
    }
    Some(resolved)
}

struct ResolvedField {
    value: String,
    parameter_pathname_pattern: bool,
    pathname_syntax: String,
}

fn resolve_word_fields(
    word: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    ifs_unknown: bool,
) -> Option<Vec<ResolvedField>> {
    if ifs_unknown {
        return None;
    }
    let ifs = assignments.get("IFS").map_or(" \t\n", String::as_str);
    let mut fields = vec![ResolvedField {
        value: String::new(),
        parameter_pathname_pattern: false,
        pathname_syntax: String::new(),
    }];
    for part in &word.parts {
        match part {
            shell::WordPart::Parameter {
                split_fields: true, ..
            } => {
                let value = resolve_word_part(part, word, assignments)?;
                append_split_fields(&mut fields, &value, ifs);
            }
            shell::WordPart::UnquotedLiteral(value) => {
                let field = fields
                    .last_mut()
                    .expect("resolved word always has a current field");
                field.value.push_str(value);
                field.pathname_syntax.push_str(value);
            }
            shell::WordPart::PathnamePattern => {}
            _ => {
                let value = resolve_word_part(part, word, assignments)?;
                let field = fields
                    .last_mut()
                    .expect("resolved word always has a current field");
                field.value.push_str(&value);
                append_quoted_pathname_text(&mut field.pathname_syntax, &value);
            }
        }
    }
    for field in &mut fields {
        field.parameter_pathname_pattern =
            shell::has_active_pathname_pattern(&field.pathname_syntax);
    }
    Some(fields)
}

fn resolve_word_part(
    part: &shell::WordPart,
    word: &shell::ShellWord,
    assignments: &HashMap<String, String>,
) -> Option<String> {
    match part {
        shell::WordPart::Literal(literal) | shell::WordPart::UnquotedLiteral(literal) => {
            Some(literal.clone())
        }
        shell::WordPart::Parameter {
            value: shell::ParameterUse::Named { name },
            ..
        } => parameter_value(name, assignments),
        shell::WordPart::Parameter {
            value:
                shell::ParameterUse::Fallback {
                    name,
                    operator,
                    test,
                    value,
                },
            ..
        } => resolve_fallback(name, *operator, *test, value, assignments),
        shell::WordPart::TildeHome => {
            let is_current_home = word.raw == "~" || word.raw.starts_with("~/");
            is_current_home
                .then(|| parameter_value("HOME", assignments))
                .flatten()
        }
        shell::WordPart::TildeOther => {
            let variable = if word.raw == "~+" || word.raw.starts_with("~+/") {
                "PWD"
            } else if word.raw == "~-" || word.raw.starts_with("~-/") {
                "OLDPWD"
            } else {
                return None;
            };
            assignments.get(variable).cloned()
        }
        _ => None,
    }
}

fn append_split_fields(fields: &mut Vec<ResolvedField>, value: &str, ifs: &str) {
    if ifs.is_empty() {
        let field = fields
            .last_mut()
            .expect("resolved word always has a current field");
        field.value.push_str(value);
        field.pathname_syntax.push_str(value);
        return;
    }
    let mut split = vec![String::new()];
    let mut previous_was_ifs_whitespace = false;
    for character in value.chars() {
        if ifs.contains(character) {
            let is_ifs_whitespace = matches!(character, ' ' | '\t' | '\n');
            if !is_ifs_whitespace || !previous_was_ifs_whitespace {
                split.push(String::new());
            }
            previous_was_ifs_whitespace = is_ifs_whitespace;
        } else {
            split
                .last_mut()
                .expect("split value always has a current field")
                .push(character);
            previous_was_ifs_whitespace = false;
        }
    }
    let first = fields
        .last_mut()
        .expect("resolved word always has a current field");
    first.value.push_str(&split[0]);
    first.pathname_syntax.push_str(&split[0]);
    fields.extend(split.into_iter().skip(1).map(|value| {
        let pathname_syntax = value.clone();
        ResolvedField {
            value,
            parameter_pathname_pattern: false,
            pathname_syntax,
        }
    }));
}

fn append_quoted_pathname_text(syntax: &mut String, text: &str) {
    for character in text.chars() {
        syntax.push('\\');
        syntax.push(character);
    }
}

fn parameter_pattern_may_supply_flag(field: &ResolvedField) -> bool {
    if !field.parameter_pathname_pattern {
        return false;
    }
    let (prefix, _) = conservative_pattern_envelope(&field.value);
    prefix.is_empty() || prefix.starts_with('-')
}

fn pattern_may_match_literal(pattern: &str, literal: &str) -> bool {
    pattern_may_match_literal_case(pattern, literal)
        || pattern_may_match_literal_case(&pattern.to_lowercase(), &literal.to_lowercase())
}

fn pattern_may_match_literal_case(pattern: &str, literal: &str) -> bool {
    let (prefix, suffix) = conservative_pattern_envelope(pattern);
    if prefix.len() == pattern.len() {
        return pattern == literal;
    }
    let suffix_may_match = suffix.is_empty() || literal.ends_with(suffix);
    suffix_may_match && literal.starts_with(prefix)
}

fn conservative_pattern_envelope(pattern: &str) -> (&str, &str) {
    let mut first = None;
    let mut last_end = 0;
    for (index, character) in pattern.char_indices() {
        if matches!(
            character,
            '*' | '?' | '[' | ']' | '(' | ')' | '!' | '+' | '@' | '\\'
        ) {
            first.get_or_insert(index);
            last_end = index + character.len_utf8();
        }
    }
    first.map_or((pattern, ""), |first| {
        (&pattern[..first], &pattern[last_end..])
    })
}

fn parameter_value(name: &str, assignments: &HashMap<String, String>) -> Option<String> {
    assignments.get(name).cloned()
}

fn resolve_fallback(
    name: &str,
    operator: shell::FallbackOperator,
    test: shell::ParameterTest,
    fallback: &shell::ShellWord,
    assignments: &HashMap<String, String>,
) -> Option<String> {
    let Some(current) = parameter_value(name, assignments) else {
        if operator == shell::FallbackOperator::Alternative {
            let alternative = resolve_word(fallback, assignments)?;
            return alternative.is_empty().then_some(alternative);
        }
        return None;
    };
    let use_default = match test {
        shell::ParameterTest::Unset => false,
        shell::ParameterTest::UnsetOrNull => current.is_empty(),
    };
    match operator {
        shell::FallbackOperator::Default | shell::FallbackOperator::AssignDefault => {
            if use_default {
                resolve_word(fallback, assignments)
            } else {
                Some(current)
            }
        }
        shell::FallbackOperator::Alternative => {
            if use_default {
                Some(String::new())
            } else {
                resolve_word(fallback, assignments)
            }
        }
    }
}

enum LeadingParameter<'a> {
    Named { name: &'a str, suffix: String },
    Fallback,
    Other,
}

fn leading_parameter(target: &shell::ShellWord) -> Option<LeadingParameter<'_>> {
    let mut parts = target.parts.iter();
    let parameter = match parts.next()? {
        shell::WordPart::Parameter {
            value: parameter, ..
        } => parameter,
        shell::WordPart::Literal(prefix) | shell::WordPart::UnquotedLiteral(prefix)
            if prefix.is_empty() =>
        {
            match parts.next()? {
                shell::WordPart::Parameter {
                    value: parameter, ..
                } => parameter,
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut suffix = String::new();
    for part in parts {
        match part {
            shell::WordPart::Literal(literal) | shell::WordPart::UnquotedLiteral(literal) => {
                suffix.push_str(literal);
            }
            _ => return Some(LeadingParameter::Other),
        }
    }
    Some(match parameter {
        shell::ParameterUse::Named { name } => LeadingParameter::Named { name, suffix },
        shell::ParameterUse::Fallback { .. } => LeadingParameter::Fallback,
        shell::ParameterUse::Other => LeadingParameter::Other,
    })
}

fn word_is_home_target(
    target: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    trusted_home: Option<&str>,
) -> bool {
    let uses_current_home = (matches!(target.parts.first(), Some(shell::WordPart::TildeHome))
        && (target.raw == "~" || target.raw.starts_with("~/")))
        || matches!(
            leading_parameter(target),
            Some(LeadingParameter::Named { name: "HOME", suffix })
                if suffix.is_empty() || suffix.starts_with('/')
        );
    if !uses_current_home {
        return false;
    }
    match assignments.get("HOME") {
        Some(home) => trusted_home
            .and_then(|trusted| lexical_absolute_parts(Path::new(trusted)))
            .zip(lexical_absolute_parts(Path::new(home)))
            .is_some_and(|(trusted, assigned)| trusted == assigned),
        None => false,
    }
}

fn word_part_is_target_dynamic(part: &shell::WordPart) -> bool {
    matches!(
        part,
        shell::WordPart::TildeHome
            | shell::WordPart::TildeOther
            | shell::WordPart::Parameter { .. }
            | shell::WordPart::AnsiCEscape
            | shell::WordPart::LocalizedText
            | shell::WordPart::PathnamePattern
            | shell::WordPart::BraceExpansion
            | shell::WordPart::ProcessSubstitution
    )
}

fn word_can_select_command(word: &shell::ShellWord) -> bool {
    word.parts.iter().any(word_part_is_target_dynamic)
}

fn word_may_not_expand_to_exactly_one_argv(word: &shell::ShellWord) -> bool {
    word.may_not_expand_to_exactly_one_argv
}

fn word_can_supply_flag(word: &shell::ShellWord) -> bool {
    word.parts.iter().any(|part| {
        matches!(
            part,
            shell::WordPart::TildeHome
                | shell::WordPart::TildeOther
                | shell::WordPart::Parameter { .. }
                | shell::WordPart::AnsiCEscape
                | shell::WordPart::LocalizedText
                | shell::WordPart::PathnamePattern
                | shell::WordPart::BraceExpansion
                | shell::WordPart::ProcessSubstitution
        )
    })
}

fn word_is_definite_target(
    word: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    ifs_unknown: bool,
    trusted_home: Option<&str>,
) -> bool {
    resolve_word(word, assignments).map_or_else(
        || word_is_home_target(word, assignments, trusted_home),
        |value| {
            if word.can_split_fields {
                resolve_word_fields(word, assignments, ifs_unknown).is_some_and(|fields| {
                    !fields.iter().any(|field| {
                        field.value.starts_with('-') || parameter_pattern_may_supply_flag(field)
                    })
                })
            } else {
                !value.starts_with('-')
            }
        },
    )
}

fn command_name(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

fn update_lastpipe_state(
    command: &str,
    arguments: &[&shell::ShellWord],
    context: shell::ExecutionContext,
    execution_context: EvalContext,
    state: &mut EvaluationState,
) {
    if execution_context != EvalContext::Caller
        || command != "shopt"
        || !matches!(
            context,
            shell::ExecutionContext::TopLevel
                | shell::ExecutionContext::Conditional
                | shell::ExecutionContext::Loop
        )
    {
        return;
    }

    let Some(arguments) = arguments
        .iter()
        .map(|argument| argument.literal.as_deref())
        .collect::<Option<Vec<_>>>()
    else {
        state.lastpipe_may_be_enabled = true;
        return;
    };
    if !arguments.contains(&"lastpipe") {
        return;
    }
    let sets = arguments.iter().any(|argument| {
        argument
            .strip_prefix('-')
            .is_some_and(|flags| flags.contains('s'))
    });
    let unsets = arguments.iter().any(|argument| {
        argument
            .strip_prefix('-')
            .is_some_and(|flags| flags.contains('u'))
    });
    if sets {
        state.lastpipe_may_be_enabled = true;
    } else if unsets && context == shell::ExecutionContext::TopLevel {
        state.lastpipe_may_be_enabled = false;
    }
}

fn update_posix_and_dispatch_state(
    command: &str,
    arguments: &[&shell::ShellWord],
    context: shell::ExecutionContext,
    execution_context: EvalContext,
    state: &mut EvaluationState,
    summary: &mut EvaluationSummary,
) -> bool {
    if execution_context != EvalContext::Caller {
        return false;
    }

    let literal_arguments = arguments
        .iter()
        .map(|argument| argument.literal.as_deref())
        .collect::<Option<Vec<_>>>();
    let mutation_affects_caller = matches!(
        context,
        shell::ExecutionContext::TopLevel
            | shell::ExecutionContext::Conditional
            | shell::ExecutionContext::Loop
    ) || (context == shell::ExecutionContext::Pipeline
        && state.lastpipe_may_be_enabled);

    if command == "set" {
        let Some(arguments) = literal_arguments.as_deref() else {
            if mutation_affects_caller {
                state.posix_mode = PosixMode::Unknown;
            }
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            return false;
        };
        let Ok((transition, posix_only)) = scan_set_posix_transition(arguments) else {
            state.posix_mode = PosixMode::Unknown;
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            return false;
        };
        if transition.is_some() && !mutation_affects_caller {
            summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
            return false;
        }
        if let Some(transition) = transition {
            let prior_mode = state.posix_mode;
            state.posix_mode = if context == shell::ExecutionContext::TopLevel {
                transition
            } else {
                PosixMode::Unknown
            };
            if context == shell::ExecutionContext::TopLevel {
                state.posix_child_mode = match transition {
                    PosixMode::Disabled => PosixMode::Disabled,
                    PosixMode::Enabled
                        if state.posix_mode_propagates && prior_mode == PosixMode::Enabled =>
                    {
                        PosixMode::Enabled
                    }
                    PosixMode::Enabled if state.posix_mode_propagates => PosixMode::Unknown,
                    PosixMode::Enabled | PosixMode::Unknown => PosixMode::Disabled,
                };
            } else {
                state.posix_child_mode = PosixMode::Unknown;
            }
        }
        return posix_only;
    }

    let dispatch_mutation = match command {
        "alias" => literal_arguments
            .as_ref()
            .is_none_or(|arguments| arguments.iter().any(|argument| argument.contains('='))),
        "unalias" => !arguments.is_empty(),
        "hash" | "enable" => !arguments.is_empty(),
        "shopt" => literal_arguments.as_ref().is_none_or(|arguments| {
            arguments.contains(&"expand_aliases")
                && arguments.iter().any(|argument| {
                    argument
                        .strip_prefix('-')
                        .is_some_and(|flags| flags.contains('s') || flags.contains('u'))
                })
        }),
        _ => false,
    };
    if dispatch_mutation {
        summary.mark_indeterminate(ShellAnalysisError::UnsupportedSyntax);
    }
    false
}

fn scan_set_posix_transition(arguments: &[&str]) -> Result<(Option<PosixMode>, bool), ()> {
    let mut transition = None;
    let mut posix_only = !arguments.is_empty();
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if *argument == "--" || *argument == "-" || !argument.starts_with(['-', '+']) {
            break;
        }
        if *argument == "+" {
            posix_only = false;
            index += 1;
            continue;
        }
        let mut flags = argument[1..].chars().peekable();
        if flags.peek().is_none() {
            break;
        }
        while let Some(flag) = flags.next() {
            if flag != 'o' {
                posix_only = false;
                if !"abefhkmnptuvxBCEHPT".contains(flag) {
                    return Err(());
                }
                continue;
            }
            if flags.peek().is_some() {
                return Err(());
            }
            let Some(option) = arguments.get(index + 1) else {
                return Ok((transition, false));
            };
            if *option != "posix" {
                return Err(());
            }
            transition = Some(if argument.starts_with('-') {
                PosixMode::Enabled
            } else {
                PosixMode::Disabled
            });
            index += 1;
        }
        index += 1;
    }
    Ok((transition, posix_only && index == arguments.len()))
}

enum CurrentShellBuiltinClassification {
    Nested(NestedExecution),
    Ordinary,
}

enum MapfileCallback {
    None,
    Literal(String),
    Unresolved,
}

fn classify_current_shell_builtin(
    words: &[&shell::ShellWord],
    context: EvalContext,
    semantics: ShellSemantics,
    prefix_assignments_persist: bool,
) -> Option<CurrentShellBuiltinClassification> {
    if context == EvalContext::External {
        return None;
    }

    let command = words.first()?.literal.as_deref()?;
    let arguments = &words[1..];
    let nested = match command {
        "source" | "." => {
            if arguments.is_empty() {
                NestedExecution::CurrentShellInert {
                    prefix_assignments_persist,
                }
            } else {
                NestedExecution::ExternalSource {
                    context,
                    prefix_assignments_persist,
                }
            }
        }
        "trap" => classify_trap(arguments, context, semantics, prefix_assignments_persist),
        "mapfile" | "readarray" => {
            return Some(match classify_mapfile_callback(arguments) {
                MapfileCallback::None => CurrentShellBuiltinClassification::Ordinary,
                MapfileCallback::Literal(program) if semantics != ShellSemantics::Portable => {
                    CurrentShellBuiltinClassification::Nested(NestedExecution::DeferredLiteral {
                        program,
                        context,
                        semantics,
                        prefix_assignments_visible: true,
                        prefix_assignments_persist: false,
                    })
                }
                MapfileCallback::Literal(_) | MapfileCallback::Unresolved => {
                    CurrentShellBuiltinClassification::Nested(NestedExecution::DeferredUnresolved {
                        error: ShellAnalysisError::UnsupportedSyntax,
                        prefix_assignments_persist: false,
                    })
                }
            });
        }
        "set" | "alias" | "unalias" | "hash" | "enable" | "shopt" => {
            return Some(CurrentShellBuiltinClassification::Ordinary);
        }
        _ => return None,
    };
    Some(CurrentShellBuiltinClassification::Nested(nested))
}

fn classify_trap(
    mut arguments: &[&shell::ShellWord],
    context: EvalContext,
    semantics: ShellSemantics,
    prefix_assignments_persist: bool,
) -> NestedExecution {
    let Some(first) = arguments.first() else {
        return NestedExecution::CurrentShellInert {
            prefix_assignments_persist,
        };
    };
    let Some(first) = first.literal.as_deref() else {
        return NestedExecution::DeferredUnresolved {
            error: ShellAnalysisError::UnsupportedSyntax,
            prefix_assignments_persist,
        };
    };

    if first == "--" {
        arguments = &arguments[1..];
    } else if first != "-"
        && let Some(flags) = first.strip_prefix('-')
    {
        if semantics == ShellSemantics::Portable
            || flags.is_empty()
            || !flags.chars().all(|flag| matches!(flag, 'l' | 'p'))
        {
            return NestedExecution::DeferredUnresolved {
                error: ShellAnalysisError::UnsupportedSyntax,
                prefix_assignments_persist,
            };
        }
        return NestedExecution::CurrentShellInert {
            prefix_assignments_persist,
        };
    }

    if arguments.is_empty() {
        return NestedExecution::CurrentShellInert {
            prefix_assignments_persist,
        };
    }
    if arguments.len() == 1 {
        return if arguments[0].literal.is_some() {
            NestedExecution::CurrentShellInert {
                prefix_assignments_persist,
            }
        } else {
            NestedExecution::DeferredUnresolved {
                error: ShellAnalysisError::UnsupportedSyntax,
                prefix_assignments_persist,
            }
        };
    }

    match arguments[0].literal.as_deref() {
        Some("") | Some("-") => NestedExecution::CurrentShellInert {
            prefix_assignments_persist,
        },
        Some(program) => NestedExecution::DeferredLiteral {
            program: program.to_string(),
            context,
            semantics,
            prefix_assignments_visible: prefix_assignments_persist,
            prefix_assignments_persist,
        },
        None => NestedExecution::DeferredUnresolved {
            error: ShellAnalysisError::UnsupportedSyntax,
            prefix_assignments_persist,
        },
    }
}

fn classify_mapfile_callback(arguments: &[&shell::ShellWord]) -> MapfileCallback {
    let mut callback = MapfileCallback::None;
    let mut index = 0;

    while let Some(argument) = arguments.get(index) {
        let Some(option) = argument.literal.as_deref() else {
            return MapfileCallback::Unresolved;
        };
        if option == "--" || option == "-" || !option.starts_with('-') {
            return callback;
        }

        let flags = &option[1..];
        if flags.is_empty() {
            return callback;
        }
        for (offset, flag) in flags.char_indices() {
            if flag == 't' {
                continue;
            }
            if !matches!(flag, 'd' | 'n' | 'O' | 's' | 'u' | 'C' | 'c') {
                return MapfileCallback::Unresolved;
            }

            let value_offset = offset + flag.len_utf8();
            let attached = &option[1 + value_offset..];
            let value = if attached.is_empty() {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return MapfileCallback::Unresolved;
                };
                value.literal.as_deref()
            } else {
                Some(attached)
            };
            let Some(value) = value else {
                callback = if flag == 'C' {
                    MapfileCallback::Unresolved
                } else {
                    return MapfileCallback::Unresolved;
                };
                break;
            };
            if matches!(flag, 'n' | 'O' | 's' | 'u' | 'c') && !valid_mapfile_number(flag, value) {
                return MapfileCallback::Unresolved;
            }
            if flag == 'C' {
                callback = MapfileCallback::Literal(value.to_string());
            }
            break;
        }
        index += 1;
    }

    callback
}

fn valid_mapfile_number(option: char, value: &str) -> bool {
    let value = value.trim_start_matches(|character: char| character.is_ascii_whitespace());
    let value = value.trim_end_matches([' ', '\t']);
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'+') => (false, &value[1..]),
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(number) = digits.parse::<u64>() else {
        return false;
    };
    if negative && number != 0 {
        return false;
    }

    let maximum = if option == 'u' {
        i32::MAX as u64
    } else {
        u32::MAX as u64
    };
    number <= maximum && (option != 'c' || number != 0)
}

fn classify_nested_execution<'a>(
    mut words: &'a [&'a shell::ShellWord],
    mut indeterminate_child_context: bool,
    mut indeterminate_after_scan: bool,
    mut eval_context: EvalContext,
    mut time_keyword_allowed: bool,
    mut eval_prefix_assignments_persist: bool,
    semantics: ShellSemantics,
) -> ClassifiedExecution<'a> {
    loop {
        let Some(command) = words.first().and_then(|word| word.literal.as_deref()) else {
            return ClassifiedExecution {
                words,
                nested: None,
                indeterminate_after_scan,
                context: eval_context,
            };
        };
        if command == "builtin" && !semantics.supports_builtin_dispatch() {
            return ClassifiedExecution {
                words,
                nested: Some(NestedExecution::EvalUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                )),
                indeterminate_after_scan,
                context: eval_context,
            };
        }
        if command != "builtin" || eval_context == EvalContext::External {
            break;
        }

        let mut dispatched = &words[1..];
        if dispatched
            .first()
            .is_some_and(|word| word.literal.as_deref() == Some("--"))
        {
            dispatched = &dispatched[1..];
        }
        let Some(selector) = dispatched.first() else {
            return ClassifiedExecution {
                words: dispatched,
                nested: Some(NestedExecution::EvalUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                )),
                indeterminate_after_scan,
                context: eval_context,
            };
        };
        let Some(selector) = selector.literal.as_deref() else {
            return ClassifiedExecution {
                words: dispatched,
                nested: Some(NestedExecution::EvalUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                )),
                indeterminate_after_scan,
                context: eval_context,
            };
        };
        if let Some(classification) =
            classify_current_shell_builtin(dispatched, eval_context, semantics, false)
        {
            match classification {
                CurrentShellBuiltinClassification::Nested(nested) => {
                    return ClassifiedExecution {
                        words: dispatched,
                        nested: Some(nested),
                        indeterminate_after_scan,
                        context: eval_context,
                    };
                }
                CurrentShellBuiltinClassification::Ordinary => {
                    words = dispatched;
                    break;
                }
            }
        }
        match selector {
            "eval" => {
                return ClassifiedExecution {
                    words: dispatched,
                    nested: Some(classify_eval_arguments(
                        &dispatched[1..],
                        eval_context,
                        eval_prefix_assignments_persist,
                    )),
                    indeterminate_after_scan,
                    context: eval_context,
                };
            }
            "builtin" => words = dispatched,
            "exec" | "command" => {
                let unwrapped = match unwrap_command_with_context(
                    dispatched,
                    eval_context,
                    time_keyword_allowed,
                    eval_prefix_assignments_persist,
                ) {
                    Ok(unwrapped) => unwrapped,
                    Err(UnwrapError::DynamicOrUnsupported) => {
                        return ClassifiedExecution {
                            words: dispatched,
                            nested: Some(NestedExecution::EvalUnresolved(
                                ShellAnalysisError::UnsupportedSyntax,
                            )),
                            indeterminate_after_scan,
                            context: eval_context,
                        };
                    }
                    Err(UnwrapError::UnsafeExpansion) => {
                        return ClassifiedExecution {
                            words: dispatched,
                            nested: Some(NestedExecution::UnsafeExpansion),
                            indeterminate_after_scan,
                            context: eval_context,
                        };
                    }
                };
                words = unwrapped.words;
                indeterminate_child_context |= unwrapped.indeterminate_child_context;
                indeterminate_after_scan |= unwrapped.indeterminate_after_scan;
                eval_context = unwrapped.eval_context;
                time_keyword_allowed = unwrapped.time_keyword_allowed;
                eval_prefix_assignments_persist = unwrapped.eval_prefix_assignments_persist;
            }
            _ => {
                return ClassifiedExecution {
                    words: dispatched,
                    nested: Some(NestedExecution::EvalUnresolved(
                        ShellAnalysisError::UnsupportedSyntax,
                    )),
                    indeterminate_after_scan,
                    context: eval_context,
                };
            }
        }
    }

    let nested = match classify_current_shell_builtin(
        words,
        eval_context,
        semantics,
        eval_prefix_assignments_persist,
    ) {
        Some(CurrentShellBuiltinClassification::Nested(nested)) => Some(nested),
        Some(CurrentShellBuiltinClassification::Ordinary) => None,
        None => match words.first().and_then(|word| word.literal.as_deref()) {
            Some("eval") if eval_context != EvalContext::External => Some(classify_eval_arguments(
                &words[1..],
                eval_context,
                eval_prefix_assignments_persist,
            )),
            Some(_) => classify_shell_invocation(words),
            None => None,
        },
    };
    let nested = nested.map(|nested| match nested {
        NestedExecution::ChildLiteral {
            program,
            semantics,
            indeterminate_after_scan: child_indeterminate_after_scan,
            ..
        } => NestedExecution::ChildLiteral {
            program,
            semantics,
            indeterminate_after_scan: child_indeterminate_after_scan || indeterminate_child_context,
            preserve_trusted_home: !indeterminate_child_context,
        },
        nested => nested,
    });
    ClassifiedExecution {
        words,
        nested,
        indeterminate_after_scan,
        context: eval_context,
    }
}

fn classify_eval_arguments(
    mut arguments: &[&shell::ShellWord],
    context: EvalContext,
    prefix_assignments_persist: bool,
) -> NestedExecution {
    if arguments
        .first()
        .is_some_and(|word| word.literal.as_deref() == Some("--"))
    {
        arguments = &arguments[1..];
    }
    arguments
        .iter()
        .map(|word| word.literal.as_deref())
        .collect::<Option<Vec<_>>>()
        .map_or(
            NestedExecution::EvalUnresolved(ShellAnalysisError::UnsupportedSyntax),
            |arguments| NestedExecution::Eval {
                program: arguments.join(" "),
                context,
                prefix_assignments_persist,
            },
        )
}

fn classify_shell_invocation(words: &[&shell::ShellWord]) -> Option<NestedExecution> {
    let command = command_name(words.first()?.literal.as_deref()?);
    if matches!(command, "zsh" | "ksh" | "mksh" | "fish") {
        return Some(NestedExecution::ChildUnresolved(
            ShellAnalysisError::UnsupportedDialect,
        ));
    }

    let (interpreter, arguments) = match command {
        "bash" | "sh" | "dash" | "ash" => (command, &words[1..]),
        "busybox" | "toybox" => {
            let Some(selector) = words.get(1) else {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            };
            let Some(selector) = selector.literal.as_deref() else {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            };
            if selector.starts_with('-') {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            }
            let supported = match command {
                "busybox" => matches!(selector, "sh" | "ash"),
                "toybox" => selector == "sh",
                _ => unreachable!("multicall registry"),
            };
            if !supported {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            }
            (selector, &words[2..])
        }
        _ => return None,
    };

    let mut semantics = if interpreter == "bash" {
        ShellSemantics::Bash
    } else {
        ShellSemantics::Portable
    };
    let mut indeterminate_after_scan = false;
    let mut index = 0;
    while let Some(option) = arguments.get(index) {
        let Some(option) = option.literal.as_deref() else {
            return Some(NestedExecution::ChildUnresolved(
                ShellAnalysisError::UnsupportedSyntax,
            ));
        };
        if interpreter == "bash" && option == "--posix" {
            semantics = ShellSemantics::BashPosix;
            index += 1;
            continue;
        }
        if option == "--" || option == "-" || !option.starts_with('-') || option.starts_with("--") {
            return Some(NestedExecution::ChildUnresolved(
                ShellAnalysisError::UnsupportedSyntax,
            ));
        }
        let flags = &option[1..];
        if flags.is_empty() || !supported_shell_flags(interpreter, flags) {
            return Some(NestedExecution::ChildUnresolved(
                ShellAnalysisError::UnsupportedSyntax,
            ));
        }
        if flags.contains('c') {
            let Some(program) = arguments.get(index + 1) else {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            };
            let Some(program) = program.literal.as_deref() else {
                return Some(NestedExecution::ChildUnresolved(
                    ShellAnalysisError::UnsupportedSyntax,
                ));
            };
            return Some(NestedExecution::ChildLiteral {
                program: program.to_string(),
                semantics,
                indeterminate_after_scan: indeterminate_after_scan || flags != "c",
                preserve_trusted_home: true,
            });
        }
        indeterminate_after_scan = true;
        index += 1;
    }

    Some(NestedExecution::ChildUnresolved(
        ShellAnalysisError::UnsupportedSyntax,
    ))
}

fn supported_shell_flags(interpreter: &str, flags: &str) -> bool {
    let supported = match interpreter {
        "bash" => "abcefhkmnptuvxBCDEHPTilrs",
        "sh" => "abcCefimnuvxs",
        "dash" => "acCefnuvxIimqVEbs",
        "ash" => "abcefinuvxs",
        _ => return false,
    };
    flags.chars().all(|flag| supported.contains(flag))
}

struct UnwrappedCommand<'a> {
    words: &'a [&'a shell::ShellWord],
    indeterminate_child_context: bool,
    indeterminate_after_scan: bool,
    eval_context: EvalContext,
    time_keyword_allowed: bool,
    eval_prefix_assignments_persist: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnwrapError {
    DynamicOrUnsupported,
    UnsafeExpansion,
}

fn unwrap_command<'a>(
    words: &'a [&'a shell::ShellWord],
    time_keyword_allowed: bool,
    eval_prefix_assignments_persist: bool,
) -> Result<UnwrappedCommand<'a>, UnwrapError> {
    unwrap_command_with_context(
        words,
        EvalContext::Caller,
        time_keyword_allowed,
        eval_prefix_assignments_persist,
    )
}

fn unwrap_command_with_context<'a>(
    mut words: &'a [&'a shell::ShellWord],
    mut eval_context: EvalContext,
    mut time_keyword_allowed: bool,
    mut eval_prefix_assignments_persist: bool,
) -> Result<UnwrappedCommand<'a>, UnwrapError> {
    let mut indeterminate_child_context = false;
    let mut indeterminate_after_scan = false;
    let mut multicall_launcher = None;
    loop {
        let Some(wrapper_word) = words.first() else {
            return Ok(UnwrappedCommand {
                words,
                indeterminate_child_context,
                indeterminate_after_scan,
                eval_context,
                time_keyword_allowed,
                eval_prefix_assignments_persist,
            });
        };
        let Some(wrapper) = wrapper_word.literal.as_deref() else {
            return if word_can_select_command(wrapper_word) {
                Err(UnwrapError::DynamicOrUnsupported)
            } else {
                Ok(UnwrappedCommand {
                    words,
                    indeterminate_child_context,
                    indeterminate_after_scan,
                    eval_context,
                    time_keyword_allowed,
                    eval_prefix_assignments_persist,
                })
            };
        };
        let wrapper_name = command_name(wrapper);
        if matches!(wrapper_name, "busybox" | "toybox")
            && words
                .get(1)
                .and_then(|selector| selector.literal.as_deref())
                .is_some_and(|selector| matches!(selector, "env" | "time"))
        {
            multicall_launcher = Some(wrapper_name);
            words = &words[1..];
            eval_context = EvalContext::External;
            time_keyword_allowed = false;
            eval_prefix_assignments_persist = false;
            continue;
        }
        match (wrapper, command_name(wrapper)) {
            (_, "time") => {
                let original_words = words;
                let multicall_launcher = multicall_launcher.take();
                let shell_keyword = wrapper == "time"
                    && (wrapper_word.raw == "time" || eval_context == EvalContext::Child)
                    && eval_context != EvalContext::External
                    && time_keyword_allowed;
                if !shell_keyword {
                    eval_context = EvalContext::External;
                    eval_prefix_assignments_persist = false;
                }
                words = &words[1..];
                while let Some(word) = words.first() {
                    match word.literal.as_deref() {
                        Some("--") => {
                            words = &words[1..];
                            break;
                        }
                        Some("-p") if shell_keyword => words = &words[1..],
                        Some(option) if shell_keyword && option.starts_with('-') => {
                            return Ok(UnwrappedCommand {
                                words: original_words,
                                indeterminate_child_context,
                                indeterminate_after_scan,
                                eval_context,
                                time_keyword_allowed,
                                eval_prefix_assignments_persist,
                            });
                        }
                        Some(option) if option.starts_with('-') && option != "-" => {
                            let option = match multicall_launcher {
                                Some("busybox") => classify_busybox_time_option(option),
                                Some("toybox") => None,
                                Some(_) => unreachable!("multicall launcher registry"),
                                None => classify_time_option(option),
                            };
                            let Some(option) = option else {
                                indeterminate_after_scan = true;
                                return Ok(UnwrappedCommand {
                                    words: original_words,
                                    indeterminate_child_context,
                                    indeterminate_after_scan,
                                    eval_context,
                                    time_keyword_allowed,
                                    eval_prefix_assignments_persist,
                                });
                            };
                            words = &words[1..];
                            match consume_wrapper_option_value(words, option) {
                                WrapperValueConsumption::Consumed(remaining) => words = remaining,
                                WrapperValueConsumption::Indeterminate => {
                                    indeterminate_after_scan = true;
                                    return Ok(UnwrappedCommand {
                                        words: original_words,
                                        indeterminate_child_context,
                                        indeterminate_after_scan,
                                        eval_context,
                                        time_keyword_allowed,
                                        eval_prefix_assignments_persist,
                                    });
                                }
                                WrapperValueConsumption::UnsafeExpansion => {
                                    return Err(UnwrapError::UnsafeExpansion);
                                }
                            }
                        }
                        None => {
                            let option = match multicall_launcher {
                                Some("busybox") => classify_dynamic_busybox_time_option(word),
                                Some("toybox") => None,
                                Some(_) => unreachable!("multicall launcher registry"),
                                None => classify_dynamic_time_option(word),
                            };
                            if let Some(option) = option {
                                words = &words[1..];
                                match consume_wrapper_option_value(words, option) {
                                    WrapperValueConsumption::Consumed(remaining) => {
                                        words = remaining
                                    }
                                    WrapperValueConsumption::Indeterminate => {
                                        indeterminate_after_scan = true;
                                        return Ok(UnwrappedCommand {
                                            words: original_words,
                                            indeterminate_child_context,
                                            indeterminate_after_scan,
                                            eval_context,
                                            time_keyword_allowed,
                                            eval_prefix_assignments_persist,
                                        });
                                    }
                                    WrapperValueConsumption::UnsafeExpansion => {
                                        return Err(UnwrapError::UnsafeExpansion);
                                    }
                                }
                                continue;
                            }
                            if word_can_select_command(word)
                                && !word.can_split_fields
                                && multicall_launcher.is_none()
                                && eval_context == EvalContext::External
                            {
                                indeterminate_after_scan = true;
                                return Ok(UnwrappedCommand {
                                    words: original_words,
                                    indeterminate_child_context,
                                    indeterminate_after_scan,
                                    eval_context,
                                    time_keyword_allowed,
                                    eval_prefix_assignments_persist,
                                });
                            }
                            if word_can_select_command(word) {
                                return Err(UnwrapError::DynamicOrUnsupported);
                            }
                            break;
                        }
                        _ => break,
                    }
                }
            }
            ("exec", _) if eval_context != EvalContext::External => {
                let original_words = words;
                eval_context = EvalContext::External;
                eval_prefix_assignments_persist = false;
                words = &words[1..];
                while let Some(option) = words.first() {
                    let Some(option) = option.literal.as_deref() else {
                        return if word_can_select_command(option) {
                            Err(UnwrapError::DynamicOrUnsupported)
                        } else {
                            Ok(UnwrappedCommand {
                                words,
                                indeterminate_child_context,
                                indeterminate_after_scan,
                                eval_context,
                                time_keyword_allowed,
                                eval_prefix_assignments_persist,
                            })
                        };
                    };
                    if option == "--" {
                        words = &words[1..];
                        break;
                    }
                    if !option.starts_with('-') || option == "-" {
                        break;
                    }
                    let Some(takes_separate_value) = classify_exec_option(option) else {
                        indeterminate_after_scan = true;
                        return Ok(UnwrappedCommand {
                            words: original_words,
                            indeterminate_child_context,
                            indeterminate_after_scan,
                            eval_context,
                            time_keyword_allowed,
                            eval_prefix_assignments_persist,
                        });
                    };
                    indeterminate_child_context = true;
                    indeterminate_after_scan = true;
                    words = &words[1..];
                    if takes_separate_value && words.is_empty() {
                        return Ok(UnwrappedCommand {
                            words,
                            indeterminate_child_context,
                            indeterminate_after_scan,
                            eval_context,
                            time_keyword_allowed,
                            eval_prefix_assignments_persist,
                        });
                    }
                    if takes_separate_value {
                        words = &words[1..];
                    }
                }
            }
            (_, "sudo") => {
                eval_context = EvalContext::External;
                eval_prefix_assignments_persist = false;
                indeterminate_child_context = true;
                words = &words[1..];
                while let Some(option) = words.first() {
                    let Some(option) = option.literal.as_deref() else {
                        return if word_can_select_command(option) {
                            Err(UnwrapError::DynamicOrUnsupported)
                        } else {
                            Ok(UnwrappedCommand {
                                words,
                                indeterminate_child_context,
                                indeterminate_after_scan,
                                eval_context,
                                time_keyword_allowed,
                                eval_prefix_assignments_persist,
                            })
                        };
                    };
                    if option == "--" {
                        words = &words[1..];
                        break;
                    }
                    if option.starts_with('-') && option != "-" {
                        let option = classify_sudo_option(option)
                            .map_err(|()| UnwrapError::DynamicOrUnsupported)?;
                        indeterminate_child_context |= option.indeterminate_child_context;
                        indeterminate_after_scan |= option.invokes_shell;
                        if option.invokes_shell {
                            eval_context = EvalContext::Child;
                            time_keyword_allowed = true;
                            eval_prefix_assignments_persist = false;
                        }
                        words = &words[1..];
                        if option.takes_separate_value && !words.is_empty() {
                            words = &words[1..];
                        }
                        continue;
                    }
                    if is_sudo_environment_argument(option) {
                        indeterminate_child_context = true;
                        words = &words[1..];
                        continue;
                    }
                    break;
                }
                indeterminate_after_scan |= words.is_empty();
            }
            ("command", _) if eval_context != EvalContext::External => {
                let original_words = words;
                let mut inspects_command = false;
                words = &words[1..];
                while let Some(word) = words.first() {
                    match word.literal.as_deref() {
                        Some("--") => {
                            words = &words[1..];
                            break;
                        }
                        Some(option) if option.starts_with('-') && option != "-" => {
                            let flags = &option[1..];
                            if !flags
                                .chars()
                                .all(|option| matches!(option, 'p' | 'v' | 'V'))
                            {
                                return Ok(UnwrappedCommand {
                                    words: original_words,
                                    indeterminate_child_context,
                                    indeterminate_after_scan,
                                    eval_context,
                                    time_keyword_allowed,
                                    eval_prefix_assignments_persist,
                                });
                            }
                            inspects_command |=
                                flags.chars().any(|option| matches!(option, 'v' | 'V'));
                            words = &words[1..];
                        }
                        None if word_can_select_command(word) => {
                            return Err(UnwrapError::DynamicOrUnsupported);
                        }
                        _ => break,
                    }
                }
                if inspects_command {
                    return Ok(UnwrappedCommand {
                        words: original_words,
                        indeterminate_child_context,
                        indeterminate_after_scan,
                        eval_context,
                        time_keyword_allowed,
                        eval_prefix_assignments_persist,
                    });
                }
                time_keyword_allowed = false;
                eval_prefix_assignments_persist = false;
            }
            (_, "env") => {
                let original_words = words;
                let multicall_launcher = multicall_launcher.take();
                eval_context = EvalContext::External;
                eval_prefix_assignments_persist = false;
                words = &words[1..];
                let mut options_ended = false;
                while let Some(word) = words.first() {
                    if !options_ended {
                        let option = match word.literal.as_deref() {
                            Some(literal)
                                if literal.starts_with('-') && !matches!(literal, "-" | "--") =>
                            {
                                match multicall_launcher {
                                    Some("busybox") => classify_busybox_env_option(literal),
                                    Some("toybox") => EnvOption::Unsupported,
                                    Some(_) => unreachable!("multicall launcher registry"),
                                    None => classify_env_option(literal),
                                }
                            }
                            None => match multicall_launcher {
                                Some("busybox") => classify_dynamic_busybox_env_option(word),
                                Some("toybox") => None,
                                Some(_) => unreachable!("multicall launcher registry"),
                                None => classify_dynamic_env_option(word),
                            }
                            .unwrap_or(EnvOption::Unsupported),
                            _ => EnvOption::Unsupported,
                        };
                        if !matches!(option, EnvOption::Unsupported)
                            || word.literal.as_deref().is_some_and(|literal| {
                                literal.starts_with('-') && !matches!(literal, "-" | "--")
                            })
                        {
                            match option {
                                EnvOption::Supported {
                                    value,
                                    child_context,
                                } => {
                                    indeterminate_child_context |= child_context;
                                    words = &words[1..];
                                    match consume_wrapper_option_value(words, value) {
                                        WrapperValueConsumption::Consumed(remaining) => {
                                            words = remaining
                                        }
                                        WrapperValueConsumption::Indeterminate => {
                                            indeterminate_after_scan = true;
                                            return Ok(UnwrappedCommand {
                                                words: original_words,
                                                indeterminate_child_context,
                                                indeterminate_after_scan,
                                                eval_context,
                                                time_keyword_allowed,
                                                eval_prefix_assignments_persist,
                                            });
                                        }
                                        WrapperValueConsumption::UnsafeExpansion => {
                                            return Err(UnwrapError::UnsafeExpansion);
                                        }
                                    }
                                    continue;
                                }
                                EnvOption::SplitString => {
                                    return Err(UnwrapError::UnsafeExpansion);
                                }
                                EnvOption::Unsupported => {
                                    indeterminate_after_scan = true;
                                    return Ok(UnwrappedCommand {
                                        words: original_words,
                                        indeterminate_child_context,
                                        indeterminate_after_scan,
                                        eval_context,
                                        time_keyword_allowed,
                                        eval_prefix_assignments_persist,
                                    });
                                }
                            }
                        }
                    }
                    let Some(literal) = word.literal.as_deref() else {
                        return if word_can_select_command(word) {
                            if multicall_launcher.is_none() && !word.can_split_fields {
                                indeterminate_after_scan = true;
                                Ok(UnwrappedCommand {
                                    words: original_words,
                                    indeterminate_child_context,
                                    indeterminate_after_scan,
                                    eval_context,
                                    time_keyword_allowed,
                                    eval_prefix_assignments_persist,
                                })
                            } else {
                                Err(UnwrapError::DynamicOrUnsupported)
                            }
                        } else {
                            Ok(UnwrappedCommand {
                                words,
                                indeterminate_child_context,
                                indeterminate_after_scan,
                                eval_context,
                                time_keyword_allowed,
                                eval_prefix_assignments_persist,
                            })
                        };
                    };
                    if !options_ended && literal == "--" {
                        options_ended = true;
                        words = &words[1..];
                    } else if !options_ended && literal == "-" {
                        indeterminate_child_context = true;
                        words = &words[1..];
                    } else if is_env_environment_argument(literal) {
                        options_ended = true;
                        indeterminate_child_context = true;
                        words = &words[1..];
                    } else if options_ended && literal.starts_with('-') {
                        indeterminate_after_scan = true;
                        return Ok(UnwrappedCommand {
                            words: original_words,
                            indeterminate_child_context,
                            indeterminate_after_scan,
                            eval_context,
                            time_keyword_allowed,
                            eval_prefix_assignments_persist,
                        });
                    } else {
                        break;
                    }
                }
            }
            _ => {
                return Ok(UnwrappedCommand {
                    words,
                    indeterminate_child_context,
                    indeterminate_after_scan,
                    eval_context,
                    time_keyword_allowed,
                    eval_prefix_assignments_persist,
                });
            }
        }
    }
}

fn is_sudo_environment_argument(word: &str) -> bool {
    word.as_bytes()
        .first()
        .is_some_and(|first| !matches!(*first, b'/' | b'='))
        && word.contains('=')
}

fn is_env_environment_argument(word: &str) -> bool {
    word.contains('=')
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WrapperOptionValue {
    None,
    Required {
        attached: WrapperOptionAttachment,
        rule: WrapperValueRule,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WrapperOptionAttachment {
    Separate,
    Literal(String),
    Dynamic {
        literal_prefix: String,
        literal_after_dynamic_proves_nonempty: bool,
        may_not_expand_to_exactly_one_argv: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapperValueRule {
    Any,
    NonEmpty,
    EnvUnsetName,
}

impl WrapperValueRule {
    fn accepts_literal(self, value: &str) -> bool {
        match self {
            Self::Any => true,
            Self::NonEmpty => !value.is_empty(),
            Self::EnvUnsetName => !value.is_empty() && !value.contains('='),
        }
    }
}

enum WrapperValueConsumption<'a> {
    Consumed(&'a [&'a shell::ShellWord]),
    Indeterminate,
    UnsafeExpansion,
}

fn consume_wrapper_option_value<'a>(
    words: &'a [&'a shell::ShellWord],
    value: WrapperOptionValue,
) -> WrapperValueConsumption<'a> {
    let WrapperOptionValue::Required { attached, rule } = value else {
        return WrapperValueConsumption::Consumed(words);
    };

    match attached {
        WrapperOptionAttachment::Literal(attached) => {
            return if rule.accepts_literal(&attached) {
                WrapperValueConsumption::Consumed(words)
            } else {
                WrapperValueConsumption::Indeterminate
            };
        }
        WrapperOptionAttachment::Dynamic {
            literal_prefix,
            literal_after_dynamic_proves_nonempty,
            may_not_expand_to_exactly_one_argv,
        } => {
            if may_not_expand_to_exactly_one_argv {
                return WrapperValueConsumption::UnsafeExpansion;
            }
            return match rule {
                WrapperValueRule::Any
                    if !literal_prefix.is_empty() || literal_after_dynamic_proves_nonempty =>
                {
                    WrapperValueConsumption::Consumed(words)
                }
                WrapperValueRule::Any
                | WrapperValueRule::NonEmpty
                | WrapperValueRule::EnvUnsetName => WrapperValueConsumption::Indeterminate,
            };
        }
        WrapperOptionAttachment::Separate => {}
    }

    let Some((word, remaining)) = words.split_first() else {
        return WrapperValueConsumption::Indeterminate;
    };
    if word_may_not_expand_to_exactly_one_argv(word) {
        return WrapperValueConsumption::UnsafeExpansion;
    }
    match word.literal.as_deref() {
        Some(literal) if rule.accepts_literal(literal) => {
            WrapperValueConsumption::Consumed(remaining)
        }
        None if rule == WrapperValueRule::Any => WrapperValueConsumption::Consumed(remaining),
        _ => WrapperValueConsumption::Indeterminate,
    }
}

fn leading_literal_option_prefix(word: &shell::ShellWord) -> Option<String> {
    if word.literal.is_some() {
        return None;
    }
    let mut prefix = String::new();
    for part in &word.parts {
        match part {
            shell::WordPart::Literal(value) | shell::WordPart::UnquotedLiteral(value) => {
                prefix.push_str(value);
            }
            _ => break,
        }
    }
    (!prefix.is_empty()).then_some(prefix)
}

fn dynamic_word_has_literal_after_dynamic(word: &shell::ShellWord) -> bool {
    let mut saw_dynamic = false;
    word.parts.iter().any(|part| match part {
        shell::WordPart::Literal(value) | shell::WordPart::UnquotedLiteral(value) => {
            saw_dynamic && !value.is_empty()
        }
        _ => {
            saw_dynamic = true;
            false
        }
    })
}

fn classify_time_option(word: &str) -> Option<WrapperOptionValue> {
    classify_time_option_prefix(word, false, false)
}

fn classify_dynamic_time_option(word: &shell::ShellWord) -> Option<WrapperOptionValue> {
    let option = classify_time_option_prefix(
        &leading_literal_option_prefix(word)?,
        true,
        word_may_not_expand_to_exactly_one_argv(word),
    );
    matches!(&option, Some(WrapperOptionValue::Required { .. })).then_some(option)?
}

fn classify_time_option_prefix(
    word: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> Option<WrapperOptionValue> {
    if word.starts_with("--") {
        return None;
    }

    let mut options = word.strip_prefix('-')?;
    if options.is_empty() {
        return None;
    }
    while let Some(option) = options.chars().next() {
        options = &options[option.len_utf8()..];
        match option {
            'o' => {
                return Some(WrapperOptionValue::Required {
                    attached: wrapper_option_attachment(
                        options,
                        dynamic_suffix,
                        may_not_expand_to_exactly_one_argv,
                    ),
                    rule: WrapperValueRule::NonEmpty,
                });
            }
            'a' | 'p' => {}
            _ => return None,
        }
    }
    Some(WrapperOptionValue::None)
}

fn classify_busybox_time_option(word: &str) -> Option<WrapperOptionValue> {
    classify_busybox_time_option_prefix(word, false, false)
}

fn classify_dynamic_busybox_time_option(word: &shell::ShellWord) -> Option<WrapperOptionValue> {
    let option = classify_busybox_time_option_prefix(
        &leading_literal_option_prefix(word)?,
        true,
        word_may_not_expand_to_exactly_one_argv(word),
    );
    matches!(&option, Some(WrapperOptionValue::Required { .. })).then_some(option)?
}

fn classify_busybox_time_option_prefix(
    word: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> Option<WrapperOptionValue> {
    let mut options = word.strip_prefix('-')?;
    if options.is_empty() {
        return None;
    }
    while let Some(option) = options.chars().next() {
        options = &options[option.len_utf8()..];
        match option {
            'o' => {
                return Some(WrapperOptionValue::Required {
                    attached: wrapper_option_attachment(
                        options,
                        dynamic_suffix,
                        may_not_expand_to_exactly_one_argv,
                    ),
                    rule: WrapperValueRule::NonEmpty,
                });
            }
            'f' => {
                return Some(WrapperOptionValue::Required {
                    attached: wrapper_option_attachment(
                        options,
                        dynamic_suffix,
                        may_not_expand_to_exactly_one_argv,
                    ),
                    rule: WrapperValueRule::Any,
                });
            }
            'a' | 'p' | 'v' => {}
            _ => return None,
        }
    }
    Some(WrapperOptionValue::None)
}

fn classify_exec_option(word: &str) -> Option<bool> {
    let mut options = word.strip_prefix('-')?.chars().peekable();
    options.peek()?;
    while let Some(option) = options.next() {
        match option {
            'c' | 'l' => {}
            'a' => return Some(options.peek().is_none()),
            _ => return None,
        }
    }
    Some(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvOption {
    Supported {
        value: WrapperOptionValue,
        child_context: bool,
    },
    SplitString,
    Unsupported,
}

fn wrapper_option_attachment(
    attached: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> WrapperOptionAttachment {
    if dynamic_suffix {
        WrapperOptionAttachment::Dynamic {
            literal_prefix: attached.into(),
            literal_after_dynamic_proves_nonempty: false,
            may_not_expand_to_exactly_one_argv,
        }
    } else if attached.is_empty() {
        WrapperOptionAttachment::Separate
    } else {
        WrapperOptionAttachment::Literal(attached.into())
    }
}

fn classify_env_option(word: &str) -> EnvOption {
    classify_env_option_prefix(word, false, false)
}

fn classify_dynamic_env_option(word: &shell::ShellWord) -> Option<EnvOption> {
    let option = classify_env_option_prefix(
        &leading_literal_option_prefix(word)?,
        true,
        word_may_not_expand_to_exactly_one_argv(word),
    );
    matches!(
        &option,
        EnvOption::Supported {
            value: WrapperOptionValue::Required { .. },
            ..
        }
    )
    .then_some(option)
}

fn classify_env_option_prefix(
    word: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> EnvOption {
    if word.starts_with("--") {
        return EnvOption::Unsupported;
    }

    let mut options = word.strip_prefix('-').unwrap_or_default();
    let mut child_context = false;
    while let Some(option) = options.chars().next() {
        options = &options[option.len_utf8()..];
        match option {
            'S' => return EnvOption::SplitString,
            'u' => {
                return EnvOption::Supported {
                    value: WrapperOptionValue::Required {
                        attached: wrapper_option_attachment(
                            options,
                            dynamic_suffix,
                            may_not_expand_to_exactly_one_argv,
                        ),
                        rule: WrapperValueRule::EnvUnsetName,
                    },
                    child_context: true,
                };
            }
            'i' => child_context = true,
            'v' => {}
            _ => return EnvOption::Unsupported,
        }
    }
    EnvOption::Supported {
        value: WrapperOptionValue::None,
        child_context,
    }
}

fn classify_busybox_env_option(word: &str) -> EnvOption {
    classify_busybox_env_option_prefix(word, false, false)
}

fn classify_dynamic_busybox_env_option(word: &shell::ShellWord) -> Option<EnvOption> {
    let mut option = classify_busybox_env_option_prefix(
        &leading_literal_option_prefix(word)?,
        true,
        word_may_not_expand_to_exactly_one_argv(word),
    );
    if let EnvOption::Supported {
        value:
            WrapperOptionValue::Required {
                attached:
                    WrapperOptionAttachment::Dynamic {
                        literal_after_dynamic_proves_nonempty,
                        ..
                    },
                rule: WrapperValueRule::Any,
            },
        ..
    } = &mut option
    {
        *literal_after_dynamic_proves_nonempty = dynamic_word_has_literal_after_dynamic(word);
    }
    matches!(
        &option,
        EnvOption::Supported {
            value: WrapperOptionValue::Required { .. },
            ..
        }
    )
    .then_some(option)
}

fn classify_busybox_env_option_prefix(
    word: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> EnvOption {
    let Some(mut options) = word.strip_prefix('-') else {
        return EnvOption::Unsupported;
    };
    if options.is_empty() {
        return EnvOption::Unsupported;
    }
    let mut child_context = false;
    while let Some(option) = options.chars().next() {
        options = &options[option.len_utf8()..];
        match option {
            'u' => {
                return EnvOption::Supported {
                    value: WrapperOptionValue::Required {
                        attached: wrapper_option_attachment(
                            options,
                            dynamic_suffix,
                            may_not_expand_to_exactly_one_argv,
                        ),
                        rule: WrapperValueRule::Any,
                    },
                    child_context: true,
                };
            }
            'i' => child_context = true,
            '0' => {}
            _ => return EnvOption::Unsupported,
        }
    }
    EnvOption::Supported {
        value: WrapperOptionValue::None,
        child_context,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SudoOptionArgument {
    None,
    Optional,
    Required,
}

struct SudoOption {
    takes_separate_value: bool,
    indeterminate_child_context: bool,
    invokes_shell: bool,
}

fn classify_sudo_option(word: &str) -> Result<SudoOption, ()> {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, None), |(name, value)| (name, Some(value)));
        let options = [
            ("background", SudoOptionArgument::None),
            ("preserve-env", SudoOptionArgument::Optional),
            ("edit", SudoOptionArgument::None),
            ("set-home", SudoOptionArgument::None),
            ("login", SudoOptionArgument::None),
            ("remove-timestamp", SudoOptionArgument::None),
            ("list", SudoOptionArgument::None),
            ("preserve-groups", SudoOptionArgument::None),
            ("shell", SudoOptionArgument::None),
            ("other-user", SudoOptionArgument::Required),
            ("validate", SudoOptionArgument::None),
            ("askpass", SudoOptionArgument::None),
            ("auth-type", SudoOptionArgument::Required),
            ("bell", SudoOptionArgument::None),
            ("close-from", SudoOptionArgument::Required),
            ("login-class", SudoOptionArgument::Required),
            ("chdir", SudoOptionArgument::Required),
            ("group", SudoOptionArgument::Required),
            ("help", SudoOptionArgument::None),
            ("host", SudoOptionArgument::Required),
            ("reset-timestamp", SudoOptionArgument::None),
            ("no-update", SudoOptionArgument::None),
            ("non-interactive", SudoOptionArgument::None),
            ("prompt", SudoOptionArgument::Required),
            ("chroot", SudoOptionArgument::Required),
            ("role", SudoOptionArgument::Required),
            ("stdin", SudoOptionArgument::None),
            ("command-timeout", SudoOptionArgument::Required),
            ("type", SudoOptionArgument::Required),
            ("user", SudoOptionArgument::Required),
            ("version", SudoOptionArgument::None),
        ];
        let (option, argument) =
            if let Some(&(option, argument)) = options.iter().find(|(option, _)| *option == name) {
                (option, argument)
            } else {
                let mut matches = options
                    .iter()
                    .filter(|(option, _)| !name.is_empty() && option.starts_with(name));
                let Some(&(option, argument)) = matches.next() else {
                    return Err(());
                };
                if matches.next().is_some() {
                    return Err(());
                }
                (option, argument)
            };
        if (attached.is_some() && argument == SudoOptionArgument::None)
            || (attached == Some("") && argument == SudoOptionArgument::Required)
        {
            return Err(());
        }
        return Ok(SudoOption {
            takes_separate_value: attached.is_none() && argument == SudoOptionArgument::Required,
            indeterminate_child_context: matches!(
                option,
                "preserve-env" | "set-home" | "login" | "user"
            ),
            invokes_shell: matches!(option, "login" | "shell"),
        });
    }

    let mut options = word.strip_prefix('-').ok_or(())?.chars().peekable();
    let mut indeterminate_child_context = false;
    let mut invokes_shell = false;
    while let Some(option) = options.next() {
        match option {
            'E' => indeterminate_child_context = true,
            'i' => {
                indeterminate_child_context = true;
                invokes_shell = true;
            }
            's' => invokes_shell = true,
            'H' => indeterminate_child_context = true,
            'A' | 'B' | 'b' | 'e' | 'K' | 'k' | 'l' | 'N' | 'n' | 'P' | 'S' | 'V' | 'v' => {}
            'u' => {
                return Ok(SudoOption {
                    takes_separate_value: options.peek().is_none(),
                    indeterminate_child_context: true,
                    invokes_shell,
                });
            }
            'a' | 'C' | 'c' | 'D' | 'g' | 'p' | 'R' | 'r' | 'T' | 't' | 'U' => {
                return Ok(SudoOption {
                    takes_separate_value: options.peek().is_none(),
                    indeterminate_child_context,
                    invokes_shell,
                });
            }
            'h' => {
                return Ok(SudoOption {
                    takes_separate_value: options.peek().is_none(),
                    indeterminate_child_context,
                    invokes_shell,
                });
            }
            _ => return Err(()),
        }
    }
    Ok(SudoOption {
        takes_separate_value: false,
        indeterminate_child_context,
        invokes_shell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn record_child_pid_before_exec(
        command: &mut std::process::Command,
        ready_file: &std::fs::File,
    ) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let ready_fd = ready_file.as_raw_fd();
        // SAFETY: the callback uses only async-signal-safe getpid/write calls,
        // and ready_file outlives the spawn performed by the evaluator.
        unsafe {
            command.pre_exec(move || {
                let bytes = libc::getpid().to_ne_bytes();
                let written = libc::write(ready_fd, bytes.as_ptr().cast(), bytes.len());
                if written == bytes.len() as isize {
                    Ok(())
                } else if written < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EIO))
                }
            });
        }
    }

    #[test]
    fn isolated_helper_protocol_accepts_canonical_responses() {
        for (rule_id, expected_reason) in [
            (
                "irreversible-root-delete",
                "refusing recursive deletion of the filesystem root",
            ),
            (
                "irreversible-home-delete",
                "refusing recursive deletion of the home directory",
            ),
            (
                "unsafe-recursive-delete-expansion",
                "refusing execution through unresolved or dynamically parsed shell input",
            ),
        ] {
            assert_eq!(
                decode_helper_response(
                    format!(r#"{{"result":"deny","rule_id":"{rule_id}"}}"#).as_bytes()
                ),
                SafetyEvaluation::Deny(SafetyDeny {
                    rule_id,
                    reason: expected_reason.into(),
                })
            );
        }
        assert_eq!(
            decode_helper_response(br#"{"result":"no_deterministic_decision"}"#),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert_eq!(
            decode_helper_response(br#"{"result":"indeterminate"}"#),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[test]
    fn isolated_helper_protocol_rejects_untrusted_responses() {
        for response in [
            br#"{"result":"deny","rule_id":"invented-rule"}"#.as_slice(),
            br#"{"result":"deny","rule_id":"invented-rule","rule_id":"irreversible-root-delete"}"#
                .as_slice(),
            br#"{"result":"allow"}"#.as_slice(),
            br#"{"result":"no_deterministic_decision","extra":true}"#.as_slice(),
            b"not-json".as_slice(),
        ] {
            assert_eq!(
                decode_helper_response(response),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn trusted_home_context_is_explicitly_bounded_and_utf8() {
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(
            validate_trusted_home_context(None),
            Err(ShellAnalysisError::HelperFailure)
        );
        assert_eq!(
            validate_trusted_home_context(Some(OsString::from("/home/alexander"))),
            Ok(OsString::from("/home/alexander"))
        );
        for invalid in ["", ".", "home/alexander"] {
            assert_eq!(
                validate_trusted_home_context(Some(OsString::from(invalid))),
                Err(ShellAnalysisError::HelperFailure),
                "{invalid:?}"
            );
        }
        assert_eq!(
            validate_trusted_home_context(Some(OsString::from("x".repeat(4_097)))),
            Err(ShellAnalysisError::HelperFailure)
        );
        assert_eq!(
            validate_trusted_home_context(Some(OsString::from_vec(vec![0xff]))),
            Err(ShellAnalysisError::HelperFailure)
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_trusted_home_stops_before_helper_construction() {
        let helper_constructed = std::cell::Cell::new(false);
        let input = ShellCommandInput {
            dialect: ShellDialect::Bash,
            source: "rm -rf /".into(),
        };

        assert_eq!(
            evaluate_isolated_with_home(&input, None, |_, _| {
                helper_constructed.set(true);
                SafetyEvaluation::NoDeterministicDecision
            }),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
        assert!(!helper_constructed.get());
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_environment_overrides_forged_markers_with_boolean_state() {
        let mut command = std::process::Command::new("/bin/true");
        command
            .env("BASH_ENV", "/tmp/attacker-startup")
            .env("BASHOPTS", "lastpipe")
            .env("SHELLOPTS", "posix")
            .env("POSIXLY_CORRECT", "1")
            .env(STARTUP_ENV_MARKER, "1")
            .env(LASTPIPE_MARKER, "1")
            .env(POSIX_MODE_ENABLED_MARKER, "1")
            .env(POSIX_MODE_UNCERTAIN_MARKER, "1")
            .env(POSIX_MODE_PROPAGATES_MARKER, "1");

        configure_isolated_helper_environment(
            &mut command,
            OsString::from("/home/trusted"),
            InheritedShellState {
                startup_environment_uncertain: false,
                lastpipe_enabled: false,
                posix_mode: PosixMode::Disabled,
                posix_mode_propagates: false,
            },
        );

        let environment = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsString::from)))
            .collect::<HashMap<_, _>>();
        assert_eq!(environment.len(), 6);
        assert_eq!(
            environment.get(&OsString::from("HOME")),
            Some(&Some(OsString::from("/home/trusted")))
        );
        assert_eq!(
            environment.get(&OsString::from(STARTUP_ENV_MARKER)),
            Some(&Some(OsString::from("0")))
        );
        assert_eq!(
            environment.get(&OsString::from(LASTPIPE_MARKER)),
            Some(&Some(OsString::from("0")))
        );
        assert_eq!(
            environment.get(&OsString::from(POSIX_MODE_ENABLED_MARKER)),
            Some(&Some(OsString::from("0")))
        );
        assert_eq!(
            environment.get(&OsString::from(POSIX_MODE_UNCERTAIN_MARKER)),
            Some(&Some(OsString::from("0")))
        );
        assert_eq!(
            environment.get(&OsString::from(POSIX_MODE_PROPAGATES_MARKER)),
            Some(&Some(OsString::from("0")))
        );
        assert!(!environment.contains_key(&OsString::from("BASH_ENV")));
        assert!(!environment.contains_key(&OsString::from("BASHOPTS")));
        assert!(!environment.contains_key(&OsString::from("SHELLOPTS")));
        assert!(!environment.contains_key(&OsString::from("POSIXLY_CORRECT")));
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_accepts_valid_json() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            r#"printf '%s\n' '{"result":"deny","rule_id":"irreversible-root-delete"}'"#,
        ]);

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(100)),
            SafetyEvaluation::Deny(SafetyDeny {
                rule_id: "irreversible-root-delete",
                reason: "refusing recursive deletion of the filesystem root".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_rejects_nonzero_exit() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(100)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_rejects_oversized_output() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "dd if=/dev/zero bs=513 count=1 2>/dev/null"]);

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(100)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_times_out_and_reaps_child() {
        let pid_file = tempfile::NamedTempFile::new().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 10"]);
        record_child_pid_before_exec(&mut command, pid_file.as_file());

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(25)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );

        let pid_bytes: [u8; size_of::<i32>()] =
            std::fs::read(pid_file.path()).unwrap().try_into().unwrap();
        let pid = i32::from_ne_bytes(pid_bytes);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_rejects_extra_output_after_json() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            r#"printf '%s' '{"result":"no_deterministic_decision"}extra'"#,
        ]);

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(100)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_helper_process_rejects_spawn_failure() {
        let mut command =
            std::process::Command::new("/definitely/missing/coding-brain-shell-safety-helper");

        assert_eq!(
            evaluate_isolated_with(&mut command, std::time::Duration::from_millis(100)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[test]
    fn isolated_helper_writes_one_bounded_json_response() {
        let mut output = Vec::new();

        run_helper_with(std::io::Cursor::new(b"rm -rf /".as_slice()), &mut output).unwrap();

        assert_eq!(
            output,
            br#"{"result":"deny","rule_id":"irreversible-root-delete"}
"#
        );
        assert!(output.len() <= MAX_HELPER_OUTPUT_BYTES);
        assert!(!String::from_utf8_lossy(&output).contains("reason"));
    }

    #[test]
    fn helper_protocol_projects_nested_deny() {
        for command in [
            "sh -c 'rm --no-preserve-root -rf /'",
            "eval -- 'rm --no-preserve-root -rf /'",
            "builtin -- eval 'rm --no-preserve-root -rf /'",
            "builtin exec sh -c 'rm --no-preserve-root -rf /'",
            "builtin command sh -c 'rm --no-preserve-root -rf /'",
            "builtin builtin eval 'rm --no-preserve-root -rf /'",
        ] {
            let mut output = Vec::new();
            run_helper_with(std::io::Cursor::new(command), &mut output).unwrap();

            assert!(
                matches!(decode_helper_response(&output), SafetyEvaluation::Deny(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn helper_protocol_projects_unresolved_nested_execution_as_indeterminate() {
        for command in [
            "sh -c \"$PROGRAM\"",
            "builtin -- eval \"$PROGRAM\"",
            "builtin exec sh -c \"$PROGRAM\"",
            "builtin command sh -c \"$PROGRAM\"",
            "builtin builtin eval \"$PROGRAM\"",
        ] {
            let mut output = Vec::new();
            run_helper_with(std::io::Cursor::new(command), &mut output).unwrap();

            assert!(
                matches!(
                    decode_helper_response(&output),
                    SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
                ),
                "{command}"
            );
        }
    }

    #[test]
    fn helper_protocol_preserves_startup_environment_uncertainty() {
        let mut output = Vec::new();
        run_helper_with_inherited(
            std::io::Cursor::new(b"printf ok"),
            &mut output,
            InheritedShellState {
                startup_environment_uncertain: true,
                lastpipe_enabled: false,
                posix_mode: PosixMode::Disabled,
                posix_mode_propagates: false,
            },
        )
        .unwrap();

        assert!(matches!(
            decode_helper_response(&output),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        ));
    }

    #[test]
    fn helper_protocol_startup_uncertainty_keeps_proven_deny_precedence() {
        let mut output = Vec::new();
        run_helper_with_inherited(
            std::io::Cursor::new(b"rm --no-preserve-root -rf /"),
            &mut output,
            InheritedShellState {
                startup_environment_uncertain: true,
                lastpipe_enabled: false,
                posix_mode: PosixMode::Disabled,
                posix_mode_propagates: false,
            },
        )
        .unwrap();

        assert!(matches!(
            decode_helper_response(&output),
            SafetyEvaluation::Deny(SafetyDeny {
                rule_id: "irreversible-root-delete",
                ..
            })
        ));
    }

    #[test]
    fn inherited_startup_uncertainty_applies_to_the_current_program() {
        let inherited = InheritedShellState {
            startup_environment_uncertain: true,
            lastpipe_enabled: false,
            posix_mode: PosixMode::Disabled,
            posix_mode_propagates: false,
        };
        let benign = ShellCommandInput {
            dialect: ShellDialect::Bash,
            source: "printf ok".into(),
        };
        let destructive = ShellCommandInput {
            dialect: ShellDialect::Bash,
            source: "rm --no-preserve-root -rf /".into(),
        };

        assert!(matches!(
            evaluate_in_process_with_inherited(Some(&benign), inherited),
            SafetyEvaluation::Indeterminate(_)
        ));
        let deny = evaluate_in_process_with_inherited(Some(&destructive), inherited);
        assert!(matches!(
            deny,
            SafetyEvaluation::Deny(SafetyDeny {
                rule_id: "irreversible-root-delete",
                ..
            })
        ));
    }

    #[test]
    fn later_startup_environment_mutation_is_not_retroactive() {
        assert_eq!(
            evaluate_result("BASH_ENV=/tmp/startup; export BASH_ENV; printf ok"),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert!(matches!(
            evaluate_result("BASH_ENV=/tmp/startup; export BASH_ENV; bash -c 'printf ok'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn native_bash_uses_bash_env_before_noninteractive_source() {
        use std::io::Write as _;

        let mut startup = tempfile::NamedTempFile::new().unwrap();
        startup.write_all(b"printf STARTUP:").unwrap();
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", "printf SOURCE"])
            .env("BASH_ENV", startup.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"STARTUP:SOURCE");
    }

    #[cfg(unix)]
    #[test]
    fn native_bash_keeps_sudo_shell_escaped_assignment_inert() {
        let escaped = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"FOO\=bar eval printf\ PAYLOAD"#,
            ])
            .output()
            .unwrap();
        let unescaped = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"FOO=bar eval printf\ CONTROL"#,
            ])
            .output()
            .unwrap();

        assert!(!escaped.status.success());
        assert!(escaped.stdout.is_empty());
        assert!(unescaped.status.success());
        assert_eq!(unescaped.stdout, b"CONTROL");
    }

    #[test]
    fn helper_protocol_preserves_inherited_lastpipe_state() {
        let mut output = Vec::new();
        run_helper_with_inherited(
            std::io::Cursor::new(
                b"TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"",
            ),
            &mut output,
            InheritedShellState {
                startup_environment_uncertain: false,
                lastpipe_enabled: true,
                posix_mode: PosixMode::Disabled,
                posix_mode_propagates: false,
            },
        )
        .unwrap();

        let deny = decode_helper_response(&output);
        assert!(matches!(
            deny,
            SafetyEvaluation::Deny(SafetyDeny {
                rule_id: "unsafe-recursive-delete-expansion",
                ..
            })
        ));
    }

    #[test]
    fn isolated_helper_rejects_oversized_input_without_output() {
        let mut output = Vec::new();

        let error = run_helper_with(
            std::io::Cursor::new(vec![b'x'; MAX_SHELL_COMMAND_BYTES + 1]),
            &mut output,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(output.is_empty());
    }

    #[test]
    fn isolated_evaluator_rejects_oversized_command() {
        let input = ShellCommandInput {
            dialect: ShellDialect::Bash,
            source: "x".repeat(MAX_SHELL_COMMAND_BYTES + 1),
        };

        assert_eq!(
            evaluate_isolated(Some(&input)),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
        );
    }

    #[test]
    fn isolated_helper_maximum_sized_literal_sequence_returns_a_bounded_no_decision() {
        let mut command = "true;".repeat(MAX_SHELL_COMMAND_BYTES / 5);
        command.push(' ');
        assert_eq!(command.len(), MAX_SHELL_COMMAND_BYTES);
        let mut output = Vec::new();

        run_helper_with(std::io::Cursor::new(command), &mut output).unwrap();

        assert_eq!(
            output,
            br#"{"result":"no_deterministic_decision"}
"#
        );
        assert!(output.len() <= MAX_HELPER_OUTPUT_BYTES);
    }

    #[test]
    fn helper_projection_of_deep_arithmetic_parentheses_does_not_split_shell_commands() {
        let depth = 128;
        let command = format!(
            "printf '%s' \"$(({}1{}))\"",
            "(".repeat(depth),
            ")".repeat(depth)
        );

        assert_eq!(
            evaluate_result(&command),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn helper_projection_of_deeply_nested_quoted_words_stops_at_the_depth_limit() {
        let mut nested = "literal".to_string();
        for _ in 0..80 {
            nested = format!("${{VALUE:-{nested}}}");
        }
        let command = format!("printf '%s' \"{nested}\"");

        assert_eq!(
            evaluate_result(&command),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit)
        );
    }

    fn evaluate_result(command: &str) -> SafetyEvaluation {
        let input = ShellCommandInput {
            dialect: ShellDialect::Bash,
            source: command.into(),
        };
        evaluate_in_process(Some(&input))
    }

    fn evaluate_command(command: &str) -> Option<SafetyDeny> {
        match evaluate_result(command) {
            SafetyEvaluation::Deny(deny) => Some(deny),
            SafetyEvaluation::NoDeterministicDecision => None,
            SafetyEvaluation::Indeterminate(error) => panic!("{command:?}: {error:?}"),
        }
    }

    struct ProcessContextGuard {
        home: Option<OsString>,
        cwd: std::path::PathBuf,
    }

    impl ProcessContextGuard {
        fn set(home: &Path, cwd: &Path) -> Self {
            let guard = Self {
                home: std::env::var_os("HOME"),
                cwd: std::env::current_dir().expect("test requires a current directory"),
            };
            std::env::set_current_dir(cwd).expect("test fixture cwd must exist");
            // SAFETY: callers hold HOME_ENV_LOCK for the guard's lifetime.
            unsafe { std::env::set_var("HOME", home) };
            guard
        }
    }

    impl Drop for ProcessContextGuard {
        fn drop(&mut self) {
            // SAFETY: callers hold HOME_ENV_LOCK for the guard's lifetime.
            unsafe {
                match self.home.take() {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
            std::env::set_current_dir(&self.cwd).expect("original test cwd must remain available");
        }
    }

    fn unrelated_pattern_prefix(home: &Path) -> String {
        let longest_component = home
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => part.to_str().map(str::len),
                _ => None,
            })
            .max()
            .expect("HOME must contain a UTF-8 component");
        "x".repeat(longest_component + 1)
    }

    #[test]
    fn unrelated_pattern_prefix_avoids_every_lexical_home_component() {
        let prefix = unrelated_pattern_prefix(Path::new("/home/xavier"));
        assert!(
            Path::new("/home/xavier")
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => part.to_str(),
                    _ => None,
                })
                .all(|component| !component.starts_with(&prefix))
        );
    }

    #[test]
    fn lexical_home_or_ancestor_classification_excludes_root_and_descendants() {
        let home = Path::new("/home/alexander");

        for target in ["/home", "/home/./alexander", "/home/alexander"] {
            assert!(
                path_is_home_or_ancestor(Path::new(target), home),
                "{target}"
            );
        }
        for target in [
            "/",
            "/hom",
            "/home/alex",
            "/home/alexander/safe",
            "/srv",
            "home/alexander",
        ] {
            assert!(
                !path_is_home_or_ancestor(Path::new(target), home),
                "{target}"
            );
        }
    }

    #[test]
    fn literal_nested_shell_destruction_denies() {
        for command in [
            "sh -c 'rm --no-preserve-root -rf /'",
            "/bin/bash -c 'rm --no-preserve-root -rf /'",
            "dash -c 'rm --no-preserve-root -rf /'",
            "ash -c 'rm --no-preserve-root -rf /'",
            "busybox sh -c 'rm --no-preserve-root -rf /'",
            "busybox ash -c 'rm --no-preserve-root -rf /'",
            "toybox sh -c 'rm --no-preserve-root -rf /'",
            "busybox env sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -i sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u HOME sh -c 'rm --no-preserve-root -rf /'",
            "busybox time sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -p sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -o log sh -c 'rm --no-preserve-root -rf /'",
            "toybox env sh -c 'rm --no-preserve-root -rf /'",
            "toybox time sh -c 'rm --no-preserve-root -rf /'",
            "busybox env toybox time sh -c 'rm --no-preserve-root -rf /'",
            "env bash -c 'rm --no-preserve-root -rf /'",
            "env X-Y=z sh -c 'rm --no-preserve-root -rf /'",
            "env 1X=z sh -c 'rm --no-preserve-root -rf /'",
            "env .X=z sh -c 'rm --no-preserve-root -rf /'",
            "env /X=z sh -c 'rm --no-preserve-root -rf /'",
            "env =X=z sh -c 'rm --no-preserve-root -rf /'",
            "env -- -X=z sh -c 'rm --no-preserve-root -rf /'",
            "sudo sh -c 'rm --no-preserve-root -rf /'",
            "sudo X-Y=z sh -c 'rm --no-preserve-root -rf /'",
            "sudo 1X=z sh -c 'rm --no-preserve-root -rf /'",
            "sudo .X=z sh -c 'rm --no-preserve-root -rf /'",
            "command bash -c 'rm --no-preserve-root -rf /'",
            "exec bash -c 'rm --no-preserve-root -rf /'",
            "time bash -c 'rm --no-preserve-root -rf /'",
            "time -- eval 'rm --no-preserve-root -rf /'",
            "time -p -- sh -c 'rm --no-preserve-root -rf /'",
            "time -- ! eval 'rm --no-preserve-root -rf /'",
            "time -p -- ! sh -c 'rm --no-preserve-root -rf /'",
            "eval 'rm --no-preserve-root -rf /'",
            "eval rm --no-preserve-root -rf /",
            "builtin eval 'rm --no-preserve-root -rf /'",
            "sh -c \"eval 'rm --no-preserve-root -rf /'\"",
            "sh -c \"bash -c 'rm --no-preserve-root -rf /'\"",
            "/usr/bin/time bash -c 'rm --no-preserve-root -rf /'",
            "command time bash -c 'rm --no-preserve-root -rf /'",
            "builtin command time bash -c 'rm --no-preserve-root -rf /'",
            "builtin exec sudo bash -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }
    }

    #[test]
    fn eval_option_terminator_is_not_program_text() {
        let deny = evaluate_command("eval -- 'rm --no-preserve-root -rf /'")
            .expect("eval must consume its option terminator before executing the command");
        assert_eq!(deny.rule_id, "irreversible-root-delete");
    }

    #[test]
    fn external_process_wrappers_cannot_grant_caller_eval_state() {
        for command in [
            "TARGET=/; sudo eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; builtin exec sudo eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; env eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; /usr/bin/time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; command time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; builtin command time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; command -v eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; builtin command -V eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; time -o log eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; sudo -s eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
            "TARGET=/; builtin exec sudo -s eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
        ] {
            assert!(
                evaluate_command(command).is_some(),
                "external eval changed caller state: {command}"
            );
        }
    }

    #[test]
    fn non_keyword_time_commands_cannot_grant_caller_eval_state() {
        for dispatch in [
            "FOO=bar time",
            ">/dev/null time",
            "'time'",
            "\\time",
            "ti'm'e",
        ] {
            let command =
                format!("TARGET=/; {dispatch} eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"");
            assert!(
                evaluate_command(&command).is_some(),
                "external time command exposed eval: {command}"
            );
        }
    }

    #[test]
    fn unsupported_command_options_cannot_grant_caller_eval_state() {
        for dispatch in [
            "command -x",
            "builtin command -x",
            "command -P",
            "builtin command --help",
            "command --verbose",
        ] {
            let command =
                format!("TARGET=/; {dispatch} eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"");
            assert!(
                evaluate_command(&command).is_some(),
                "unsupported command option exposed eval: {command}"
            );
        }
    }

    #[test]
    fn command_operands_that_look_like_assignments_do_not_expose_eval() {
        for dispatch in [
            "command FOO=bar",
            "builtin command FOO=bar",
            "command -- FOO=bar",
            "command -p FOO=bar",
            "command command FOO=bar",
            "command time FOO=bar",
        ] {
            let command =
                format!("TARGET=/; {dispatch} eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"");
            assert!(
                evaluate_command(&command).is_some(),
                "assignment-looking command operand exposed eval: {command}"
            );
        }
    }

    #[test]
    fn syntactic_assignment_lookalike_commands_cannot_expose_caller_eval() {
        for dispatch in [
            "'FOO=bar'",
            "FOO\\=bar",
            "F'O'O=bar",
            "./FOO=bar",
            "time 'FOO=bar'",
            "time time FOO\\=bar",
        ] {
            let command =
                format!("TARGET=/; {dispatch} eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"");
            let deny = evaluate_command(&command)
                .unwrap_or_else(|| panic!("assignment lookalike exposed eval: {command}"));
            assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
        }
    }

    #[test]
    fn syntactic_assignment_lookalike_commands_cannot_expose_caller_shopt() {
        for dispatch in [
            "'FOO=bar'",
            "FOO\\=bar",
            "F'O'O=bar",
            "./FOO=bar",
            "time 'FOO=bar'",
            "time time FOO\\=bar",
        ] {
            let command = format!(
                "{dispatch} shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${{X:+-rf}}\" /"
            );
            assert_eq!(
                evaluate_result(&command),
                SafetyEvaluation::NoDeterministicDecision,
                "assignment lookalike exposed shopt: {command}"
            );
        }
    }

    #[test]
    fn external_assignment_lookalike_commands_keep_payloads_inert() {
        for command in [
            "exec 'FOO=bar' eval 'rm --no-preserve-root -rf /'",
            "/usr/bin/time FOO\\=bar eval 'rm --no-preserve-root -rf /'",
            "env -- FOO=bar eval 'rm --no-preserve-root -rf /'",
            "sudo -- 'FOO=bar' eval 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        for command in [
            "sudo -s -- FOO=bar eval 'rm --no-preserve-root -rf /'",
            "sudo -s -- 'FOO=bar' eval 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn genuine_prefix_and_wrapper_assignments_remain_structural() {
        for command in [
            "MODE=fast shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "time -- MODE=fast shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "env MODE=fast sh -c 'rm --no-preserve-root -rf /'",
            "sudo MODE=fast sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "unsafe-recursive-delete-expansion" | "irreversible-root-delete"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }
    }

    #[test]
    fn caller_shell_eval_dispatch_still_updates_caller_state() {
        for dispatch in [
            "eval",
            "command eval",
            "command -p eval",
            "command -- eval",
            "builtin command eval",
            "builtin command -p eval",
            "time eval",
            "time command eval",
            "! time eval",
            "time -- time eval",
        ] {
            let command = format!("TARGET=/; {dispatch} 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"");
            assert_eq!(
                evaluate_result(&command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn posix_child_eval_prefix_assignments_persist() {
        for interpreter in [
            "sh",
            "dash",
            "ash",
            "busybox sh",
            "busybox ash",
            "toybox sh",
            "bash --posix",
        ] {
            let command = format!(
                "{interpreter} -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; \
                 rm --no-preserve-root -rf \"$TARGET\"'"
            );
            let deny = evaluate_command(&command)
                .unwrap_or_else(|| panic!("POSIX eval prefix assignment escaped: {command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }

        for command in [
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ builtin eval \":\"; \
             rm --no-preserve-root -rf \"$TARGET\"'",
            "bash --posix -c 'TARGET=/tmp/safe; time TARGET=/ eval \":\"; \
             rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert!(
                evaluate_command(command).is_some(),
                "Bash POSIX special builtin assignment escaped: {command}"
            );
        }
    }

    #[test]
    fn posix_command_eval_prefix_assignments_are_temporary() {
        for interpreter in ["sh", "dash", "ash", "bash --posix"] {
            let command = format!(
                "{interpreter} -c 'TARGET=/tmp/safe; TARGET=/ command eval \":\"; \
                 rm --no-preserve-root -rf \"$TARGET\"'"
            );
            assert_eq!(
                evaluate_result(&command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn portable_children_do_not_trust_bash_only_time_or_builtin_dispatch() {
        for interpreter in [
            "sh",
            "dash",
            "ash",
            "busybox sh",
            "busybox ash",
            "toybox sh",
        ] {
            for dispatch in ["time eval", "builtin eval"] {
                let command = format!(
                    "{interpreter} -c 'TARGET=/; {dispatch} \"TARGET=/tmp/safe\"; \
                     rm --no-preserve-root -rf \"$TARGET\"'"
                );
                assert!(
                    evaluate_command(&command).is_some(),
                    "portable child trusted Bash-only dispatch: {command}"
                );
            }

            let direct = format!("{interpreter} -c 'builtin eval \"rm --no-preserve-root -rf /\"'");
            assert!(
                matches!(
                    evaluate_result(&direct),
                    SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
                ),
                "portable builtin use was trusted: {direct}"
            );
        }
    }

    #[test]
    fn default_bash_child_eval_prefix_assignments_are_temporary() {
        for command in [
            "bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; \
             rm --no-preserve-root -rf \"$TARGET\"'",
            "bash -c 'TARGET=/tmp/safe; time TARGET=/ eval \":\"; \
             rm --no-preserve-root -rf \"$TARGET\"'",
            "bash -c 'TARGET=/tmp/safe; TARGET=/ builtin eval \":\"; \
             rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn external_eval_lookalikes_are_not_nested_programs() {
        for command in [
            "sudo eval 'rm --no-preserve-root -rf /'",
            "env eval 'rm --no-preserve-root -rf /'",
            "/usr/bin/time eval 'rm --no-preserve-root -rf /'",
            "command time eval 'rm --no-preserve-root -rf /'",
            "command -v eval 'rm --no-preserve-root -rf /'",
            "builtin command -V eval 'rm --no-preserve-root -rf /'",
            "command -x eval 'rm --no-preserve-root -rf /'",
            "builtin command -P eval 'rm --no-preserve-root -rf /'",
            "command --help eval 'rm --no-preserve-root -rf /'",
            "command --verbose eval 'rm --no-preserve-root -rf /'",
            "command FOO=bar eval 'rm --no-preserve-root -rf /'",
            "builtin command -- FOO=bar eval 'rm --no-preserve-root -rf /'",
            "command time FOO=bar eval 'rm --no-preserve-root -rf /'",
            "time -o log eval 'rm --no-preserve-root -rf /'",
            "exec eval 'rm --no-preserve-root -rf /'",
            "builtin exec eval 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn command_prefix_assignments_flow_into_eval_without_escaping_caller_state() {
        for dispatch in [
            "eval",
            "builtin eval",
            "builtin -- eval",
            "command eval",
            "builtin command eval",
            "builtin builtin eval",
        ] {
            let destructive = format!(
                "TARGET=/tmp/safe; TARGET=/ {dispatch} \
                 'rm --no-preserve-root -rf \"$TARGET\"'"
            );
            let deny = evaluate_command(&destructive)
                .unwrap_or_else(|| panic!("eval prefix assignment escaped safety: {destructive}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
                ),
                "{destructive}: {}",
                deny.rule_id
            );

            let safe =
                format!("TARGET=/tmp/safe; TARGET=/ {dispatch} 'printf ok'; rm -rf \"$TARGET\"");
            assert_eq!(
                evaluate_result(&safe),
                SafetyEvaluation::NoDeterministicDecision,
                "{safe}"
            );
        }
    }

    #[test]
    fn command_prefix_assignment_uncertainty_fails_closed_for_eval() {
        assert!(matches!(
            evaluate_result(
                "TARGET=/tmp/safe; TARGET=\"$UNKNOWN\" eval 'printf ok'; \
                 rm -rf \"$TARGET\""
            ),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn prefixed_eval_still_propagates_other_caller_state_changes() {
        let deny = evaluate_command(
            "TARGET=/tmp/safe; TARGET=/ eval 'NEXT=/'; \
             rm --no-preserve-root -rf \"$NEXT\"",
        )
        .expect("non-prefix eval assignments must still flow to the caller");
        assert!(matches!(
            deny.rule_id,
            "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
        ));
    }

    #[test]
    fn prefixed_eval_never_restores_a_stale_value_after_same_name_mutation() {
        for dispatch in [
            "eval",
            "builtin eval",
            "builtin -- eval",
            "command eval",
            "builtin command eval",
            "builtin builtin eval",
        ] {
            let command = format!(
                "TARGET=/tmp/safe; TARGET=/ {dispatch} 'TARGET=/'; \
                 rm --no-preserve-root -rf \"$TARGET\""
            );
            assert!(
                evaluate_command(&command).is_some(),
                "same-name eval mutation restored stale safety state: {command}"
            );
        }
    }

    #[test]
    fn prefixed_eval_restores_the_temporary_name_after_generic_invalidation() {
        assert_eq!(
            evaluate_result(
                "TARGET=/tmp/safe; TARGET=/ eval 'arbitrary_mutator'; \
                 rm -rf \"$TARGET\""
            ),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn builtin_dispatch_preserves_supported_nested_execution() {
        for command in [
            "builtin -- eval 'rm --no-preserve-root -rf /'",
            "builtin exec sh -c 'rm --no-preserve-root -rf /'",
            "builtin command sh -c 'rm --no-preserve-root -rf /'",
            "builtin builtin eval 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("supported builtin dispatch escaped safety: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn builtin_dispatch_uncertainty_fails_closed() {
        for command in [
            "builtin -- eval \"$UNKNOWN\"",
            "builtin exec sh -c \"$UNKNOWN\"",
            "builtin command sh -c \"$UNKNOWN\"",
            "builtin builtin eval \"$UNKNOWN\"",
            "builtin \"$SELECTOR\" 'printf ok'",
            "builtin unknown-selector 'printf ok'",
            "builtin /tmp/eval 'printf ok'",
            "builtin ./exec sh -c 'printf ok'",
            "builtin path/command sh -c 'printf ok'",
            "builtin path/builtin eval 'printf ok'",
            "builtin",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn builtin_dispatch_benign_and_inert_controls_stay_non_deterministic() {
        for command in [
            "builtin -- eval 'printf ok'",
            "builtin exec sh -c 'printf ok'",
            "builtin command sh -c 'printf ok'",
            "builtin builtin eval 'printf ok'",
            "printf '%s' \"builtin exec sh -c 'rm -rf /'\"",
            "printf '%s' \"builtin -- eval 'rm -rf /'\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn execution_bearing_source_and_dot_abstain_without_reading_external_content() {
        for command in [
            "source /definitely/not/read-by-safety",
            ". /definitely/not/read-by-safety",
            "'source' /definitely/not/read-by-safety",
            "command source /definitely/not/read-by-safety",
            "command -- . /definitely/not/read-by-safety",
            "builtin source /definitely/not/read-by-safety",
            "builtin -- . /definitely/not/read-by-safety",
            "source /dev/stdin",
            ". /dev/fd/0",
            "source \"$FILE\"",
            "bash -c 'source /definitely/not/read-by-safety'",
            "bash --posix -c '. /definitely/not/read-by-safety'",
            "sh -c '. /definitely/not/read-by-safety'",
            "sh -c 'source /definitely/not/read-by-safety'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn source_and_dot_inert_controls_preserve_caller_state() {
        for command in [
            "TARGET=/tmp/safe; source; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; .; rm -rf \"$TARGET\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        for command in [
            "TARGET=/tmp/safe; /tmp/source file; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; ./source file; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; env source file; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; exec source file; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; printf '%s' 'source /tmp/file'; rm -rf \"$TARGET\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| {
                panic!("ordinary external command skipped invalidation: {command}")
            });
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn sourced_program_boundaries_cover_posix_prefix_state_without_reads() {
        for command in [
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ . /dev/null; rm --no-preserve-root -rf \"$TARGET\"'",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ source /dev/null; rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert!(evaluate_command(command).is_some(), "{command}");
        }

        for command in [
            "bash --posix -c 'TARGET=/ command . /dev/null'",
            "bash --posix -c 'TARGET=/ builtin . /dev/null'",
            "bash --posix -c 'TARGET=/ command source /dev/null'",
            "bash --posix -c 'TARGET=/ builtin source /dev/null'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn trap_literal_actions_are_recursively_scanned_before_abstention() {
        for command in [
            "trap 'rm --no-preserve-root -rf /' EXIT",
            "trap -- 'rm --no-preserve-root -rf /' 0",
            "builtin trap 'rm --no-preserve-root -rf /' EXIT",
            "builtin -- trap 'rm --no-preserve-root -rf /' DEBUG",
            "command trap 'rm --no-preserve-root -rf /' EXIT",
            "trap 'rm --no-preserve-root -rf /' \"$SIGNAL\"",
            "bash -c \"trap 'rm --no-preserve-root -rf /' EXIT\"",
            "sh -c \"trap 'rm --no-preserve-root -rf /' EXIT\"",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("trap action escaped recursive scan: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
        let command = "TARGET=/; trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT";
        assert!(evaluate_command(command).is_some(), "{command}");

        for command in [
            "trap ':' EXIT",
            "trap \"$ACTION\" EXIT",
            "trap 'printf ok' \"$SIGNAL\"",
            "trap -x 'printf ok' EXIT",
            "trap -P EXIT",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn trap_query_list_reset_and_ignore_forms_are_inert() {
        for command in [
            "trap",
            "trap -p",
            "trap -p EXIT",
            "trap -p \"$SIGNAL\"",
            "trap -l",
            "trap -lp",
            "trap EXIT",
            "trap - EXIT",
            "trap '' EXIT",
            "trap --",
            "trap -- - EXIT",
            "trap -- '' EXIT",
            "builtin trap -p EXIT",
            "builtin -- trap -l",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        for command in [
            "TARGET=/tmp/safe; trap -p EXIT; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; trap - EXIT; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; trap '' EXIT; rm -rf \"$TARGET\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "trap inert form mutated caller state: {command}"
            );
        }
    }

    #[test]
    fn trap_uncertainty_preserves_state_and_visible_deny_precedence() {
        assert!(matches!(
            evaluate_result("TARGET=/tmp/safe; trap ':' EXIT; rm -rf \"$TARGET\""),
            SafetyEvaluation::Indeterminate(_)
        ));
        for command in [
            "trap ':' EXIT; rm --no-preserve-root -rf /",
            "rm --no-preserve-root -rf /; trap \"$ACTION\" EXIT",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn trap_special_builtin_prefix_state_respects_shell_mode_and_wrappers() {
        for command in [
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ trap -p EXIT; rm --no-preserve-root -rf \"$TARGET\"'",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ trap - EXIT; rm --no-preserve-root -rf \"$TARGET\"'",
            "sh -c 'TARGET=/tmp/safe; TARGET=/ trap \"\" EXIT; rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert!(evaluate_command(command).is_some(), "{command}");
        }

        for command in [
            "TARGET=/tmp/safe; TARGET=/ trap -p EXIT; rm --no-preserve-root -rf \"$TARGET\"",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ command trap -p EXIT; rm --no-preserve-root -rf \"$TARGET\"'",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ builtin trap -p EXIT; rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        assert!(matches!(
            evaluate_result("sh -c 'trap -p EXIT'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn trap_actions_observe_prefix_state_when_the_deferred_action_runs() {
        for command in [
            "TARGET=/; TARGET=/tmp/safe trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "TARGET=/; TARGET=/tmp/safe command trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "TARGET=/; TARGET=/tmp/safe builtin trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "bash --posix -c \"TARGET=/; TARGET=/tmp/safe command trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
            "bash --posix -c \"TARGET=/; TARGET=/tmp/safe builtin trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("restored trap state escaped scan: {command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }

        for command in [
            "TARGET=/tmp/safe; TARGET=/ trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "TARGET=/tmp/safe; TARGET=/ command trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "TARGET=/tmp/safe; TARGET=/ builtin trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
            "bash --posix -c \"TARGET=/tmp/safe; TARGET=/ command trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
            "bash --posix -c \"TARGET=/tmp/safe; TARGET=/ builtin trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "temporary trap prefix leaked into deferred action: {command}"
            );
        }

        for command in [
            "bash --posix -c \"TARGET=/tmp/safe; TARGET=/ trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
            "sh -c \"TARGET=/tmp/safe; TARGET=/ trap 'rm --no-preserve-root -rf \\\"\\$TARGET\\\"' EXIT\"",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("persistent trap prefix escaped scan: {command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }
    }

    #[test]
    fn mapfile_and_readarray_literal_callbacks_are_recursively_scanned() {
        for command in [
            "mapfile -c 1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -c1 -C'rm --no-preserve-root -rf /'",
            "mapfile -tc1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -C 'rm --no-preserve-root -rf /' -c1",
            "mapfile -d '' -n 1 -O 0 -s 0 -u 0 -tc1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -C 'rm --no-preserve-root -rf /' -- MAPFILE",
            "readarray -c1 -C 'rm --no-preserve-root -rf /'",
            "builtin mapfile -c1 -C 'rm --no-preserve-root -rf /'",
            "builtin -- readarray -C'rm --no-preserve-root -rf /' -c1",
            "command mapfile -c1 -C 'rm --no-preserve-root -rf /'",
            "bash -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
            "bash --posix -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
            "mapfile -C ':' -C 'rm --no-preserve-root -rf /' -c1",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("mapfile callback escaped recursive scan: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
        for command in [
            "TARGET=/ mapfile -c1 -C 'rm --no-preserve-root -rf \"$TARGET\"'",
            "TARGET=/; mapfile -c1 -C 'rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert!(evaluate_command(command).is_some(), "{command}");
        }

        for command in [
            "mapfile -c1 -C ':'",
            "readarray -C \"$CALLBACK\" -c1",
            "mapfile \"$OPTIONS\"",
            "mapfile -C",
            "mapfile -c",
            "mapfile -c nope -C ':'",
            "mapfile -n \"$COUNT\" -C ':'",
            "mapfile -Z -C ':'",
            "mapfile -C 'rm --no-preserve-root -rf /' -C ':' -c1",
            "sh -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn mapfile_numeric_operands_follow_bash_ranges_and_signs() {
        for command in [
            "mapfile -c +1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -c+1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -n +0 -O -0 -s +0 -u +0 -c +01 -C 'rm --no-preserve-root -rf /'",
            "mapfile -n 4294967295 -c 1 -C 'rm --no-preserve-root -rf /'",
            "mapfile -c '\n+1\t ' -C 'rm --no-preserve-root -rf /'",
            "mapfile -c '\r+1\t ' -C 'rm --no-preserve-root -rf /'",
            "mapfile -c '\u{000c}+1\t ' -C 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("valid Bash numeric operand escaped scan: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        for command in [
            "mapfile -C 'rm --no-preserve-root -rf /' -c 0",
            "mapfile -C 'rm --no-preserve-root -rf /' -c -0",
            "mapfile -C 'rm --no-preserve-root -rf /' -n 4294967296",
            "mapfile -C 'rm --no-preserve-root -rf /' -O 4294967296",
            "mapfile -C 'rm --no-preserve-root -rf /' -s 4294967296",
            "mapfile -C 'rm --no-preserve-root -rf /' -u 2147483648",
            "mapfile -C 'rm --no-preserve-root -rf /' -c 4294967296",
            "mapfile -C 'rm --no-preserve-root -rf /' -c '1\n'",
            "mapfile -C 'rm --no-preserve-root -rf /' -c '1\r'",
            "mapfile -C 'rm --no-preserve-root -rf /' -c '1\u{000c}'",
            "mapfile -C 'rm --no-preserve-root -rf /' -c '1\u{000b}'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "invalid Bash numeric operand reached callback scan: {command}"
            );
        }
    }

    #[test]
    fn callback_free_mapfile_controls_keep_existing_state_behavior() {
        for command in [
            "mapfile",
            "readarray -c 1",
            "mapfile -- MAPFILE",
            "mapfile -- '-Cprintf DANGER'",
            "readarray -c1 -- '-Cprintf DANGER'",
            "mapfile -- \"$OPTIONS\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        let deny = evaluate_command("TARGET=/tmp/safe; mapfile; rm -rf \"$TARGET\"")
            .expect("plain mapfile must retain ordinary state invalidation");
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");

        assert!(matches!(
            evaluate_result("TARGET=/tmp/safe; mapfile -c1 -C ':'; rm -rf \"$TARGET\""),
            SafetyEvaluation::Indeterminate(_)
        ));
        assert!(matches!(
            evaluate_result(
                "TARGET=/tmp/safe; bash -c \"mapfile -c1 -C 'TARGET=/'\"; rm -rf \"$TARGET\""
            ),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn mapfile_callbacks_keep_prefix_assignments_temporary_in_bash_modes() {
        for command in [
            "TARGET=/tmp/safe; TARGET=/ mapfile -c1 -C ':' </dev/null; rm --no-preserve-root -rf \"$TARGET\"",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ mapfile -c1 -C \":\" </dev/null; rm --no-preserve-root -rf \"$TARGET\"'",
            "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ readarray -c1 -C \":\" </dev/null; rm --no-preserve-root -rf \"$TARGET\"'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn path_like_command_words_do_not_select_shell_builtins() {
        for command in [
            "/tmp/builtin eval 'rm --no-preserve-root -rf /'",
            "./eval 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn path_like_shell_builtin_wrappers_are_not_unwrapped() {
        for command in [
            "/tmp/command sh -c 'rm --no-preserve-root -rf /'",
            "./exec sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn absolute_registered_child_shell_remains_supported() {
        let deny = evaluate_command("/bin/sh -c 'rm --no-preserve-root -rf /'")
            .expect("approved absolute child-shell basename must remain supported");
        assert_eq!(deny.rule_id, "irreversible-root-delete");
    }

    #[test]
    fn inert_nested_shell_text_stays_non_executable() {
        for command in [
            "printf '%s' \"sh -c 'rm -rf /'\"",
            "printf '%s' \"eval 'rm -rf /'\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn unresolved_nested_execution_is_indeterminate() {
        for command in [
            "sh -c \"$PROGRAM\"",
            "bash script.sh",
            "printf program | bash",
            "bash <<< 'printf ok'",
            "zsh -c 'printf ok'",
            "bash -lc 'printf ok'",
            "bash --rcfile profile -c 'printf ok'",
            "busybox --help sh -c 'printf ok'",
            "bash -c",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn explicit_child_shell_environment_is_indeterminate_after_benign_scan() {
        for command in [
            "BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
            "SAFE_VALUE=known sh -c 'printf ok'",
            "env BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
            "env -- BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
            "env - BASH_ENV=/tmp/attacker-startup /bin/sh -c 'printf ok'",
            "sudo BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn explicit_child_shell_environment_still_scans_proven_destruction() {
        for command in [
            "BASH_ENV=/tmp/attacker-startup bash -c 'rm --no-preserve-root -rf /'",
            "env BASH_ENV=/tmp/attacker-startup bash -c 'rm --no-preserve-root -rf /'",
            "env -- BASH_ENV=/tmp/attacker-startup bash -c 'rm --no-preserve-root -rf /'",
            "sudo BASH_ENV=/tmp/attacker-startup bash -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("environment metadata hid destruction: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn env_argv0_forms_preserve_child_execution_and_uncertainty() {
        for command in [
            "env -a displayed bash -c 'printf ok'",
            "env -adisplayed bash -c 'printf ok'",
            "env -ia displayed /bin/bash -c 'printf ok'",
            "env --argv0=displayed bash -c 'printf ok'",
            "env --argv0 displayed /bin/bash -c 'printf ok'",
            "env -a displayed bash -c 'rm --no-preserve-root -rf /'",
            "env -adisplayed /bin/bash -c 'rm --no-preserve-root -rf /'",
            "env --argv0=displayed bash -c 'rm --no-preserve-root -rf /'",
            "env --argv0 displayed /bin/bash -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        for command in ["env -a", "env --argv0"] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn exec_exact_options_preserve_child_execution_and_uncertainty() {
        for command in [
            "exec -a displayed bash -c 'printf ok'",
            "exec -c bash -c 'printf ok'",
            "exec -l bash -c 'printf ok'",
            "exec -cla displayed /bin/bash -c 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        for command in [
            "exec -a displayed bash -c 'rm --no-preserve-root -rf /'",
            "exec -c bash -c 'rm --no-preserve-root -rf /'",
            "exec -l /bin/bash -c 'rm --no-preserve-root -rf /'",
            "exec -cla displayed bash -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).expect("exec metadata must not hide destruction");
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        for command in [
            "exec FOO=bar bash -c 'rm --no-preserve-root -rf /'",
            "exec -- FOO=bar bash -c 'rm --no-preserve-root -rf /'",
            "./exec -a displayed bash -c 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn sudo_child_context_options_are_indeterminate_after_benign_scan() {
        for command in [
            "sudo -E bash -c 'printf ok'",
            "sudo -nE bash -c 'printf ok'",
            "sudo -En bash -c 'printf ok'",
            "sudo --preserve-env bash -c 'printf ok'",
            "sudo --preserve-env=BASH_ENV,SHELL bash -c 'printf ok'",
            "sudo --preserve-e=BASH_ENV bash -c 'printf ok'",
            "sudo -uE bash -c 'printf ok'",
            "sudo -u E bash -c 'printf ok'",
            "sudo --user=E bash -c 'printf ok'",
            "sudo -i bash -c 'printf ok'",
            "sudo -ni bash -c 'printf ok'",
            "sudo --login bash -c 'printf ok'",
            "sudo -i printf ok",
            "sudo --login printf ok",
            "sudo -s bash -c 'printf ok'",
            "sudo -ns bash -c 'printf ok'",
            "sudo --shell bash -c 'printf ok'",
            "sudo --sh bash -c 'printf ok'",
            "sudo -s printf ok",
            "sudo -ns printf ok",
            "sudo --shell printf ok",
            "/usr/bin/sudo -E /bin/bash -c 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn sudo_environment_boundaries_do_not_seed_caller_home_into_child_shells() {
        for command in [
            "sudo bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
            "sudo -H bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
            "sudo --set-home bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
            "sudo -u root bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
            "sudo --user=root bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
            "sudo -i bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
        ] {
            let deny = evaluate_command(command).expect("sudo child HOME must fail closed");
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        assert!(matches!(
            evaluate_result("sudo bash -c 'rm --no-preserve-root -rf /root'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn sudo_child_context_options_still_scan_proven_destruction() {
        for command in [
            "sudo -nE bash -c 'rm --no-preserve-root -rf /'",
            "sudo --preserve-env=BASH_ENV bash -c 'rm --no-preserve-root -rf /'",
            "sudo -ni bash -c 'rm --no-preserve-root -rf /'",
            "sudo --login bash -c 'rm --no-preserve-root -rf /'",
            "sudo -ns bash -c 'rm --no-preserve-root -rf /'",
            "sudo --shell bash -c 'rm --no-preserve-root -rf /'",
            "sudo -i rm --no-preserve-root -rf /",
            "sudo --shell rm --no-preserve-root -rf /",
            "builtin exec sudo --shell rm --no-preserve-root -rf /",
            "command sudo -s time eval 'rm --no-preserve-root -rf /'",
            "builtin command sudo -i time eval 'rm --no-preserve-root -rf /'",
            "sudo -s 'time' eval 'rm --no-preserve-root -rf /'",
            "sudo -i \\time eval 'rm --no-preserve-root -rf /'",
            "sudo -s ti'm'e eval 'rm --no-preserve-root -rf /'",
            "sudo X-Y=z -s eval 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("sudo context metadata hid destruction: {command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn sudo_option_terminator_keeps_assignment_looking_external_command() {
        for command in [
            "sudo -- FOO=bar sh -c 'rm --no-preserve-root -rf /'",
            "sudo -- X-Y=z sh -c 'rm --no-preserve-root -rf /'",
            "sudo /X=z sh -c 'rm --no-preserve-root -rf /'",
            "sudo =X=z sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn sudo_implicit_shell_execution_is_indeterminate() {
        for command in [
            "sudo",
            "sudo -E",
            "sudo --preserve-env",
            "sudo BASH_ENV=/tmp/attacker-startup",
            "sudo -i",
            "sudo -s",
            "sudo -s '$DANGER'",
            "sudo --shell '$DANGER'",
            "sudo -i '$DANGER'",
            "sudo --login '$DANGER'",
            "sudo -ns '$DANGER'",
            "sudo --shell 'rm${SEP}-rf' /",
            "sudo DANGER='rm --no-preserve-root -rf /' -s '$DANGER'",
            "sudo DANGER='rm --no-preserve-root -rf /' --login '$DANGER'",
            "builtin command sudo -i printf ok",
            "builtin exec sudo -s '$DANGER'",
            "builtin exec sudo -E",
            "builtin builtin exec sudo DANGER='rm --no-preserve-root -rf /' -s '$DANGER'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn sudo_context_option_lookalikes_do_not_create_metadata() {
        for command in [
            "sudo -pE printf ok",
            "sudo -p -E printf ok",
            "sudo -hE printf ok",
            "sudo -h E printf ok",
            "sudo --prompt=-E printf ok",
            "printf '%s' \"sudo -E bash -c 'rm --no-preserve-root -rf /'\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn unknown_dynamic_and_ambiguous_sudo_options_fail_closed() {
        for command in [
            "sudo --unknown bash -c 'printf ok'",
            "sudo --pre bash -c 'printf ok'",
            "sudo --s bash -c 'printf ok'",
            "sudo \"$OPTIONS\" bash -c 'printf ok'",
        ] {
            assert!(evaluate_command(command).is_some(), "{command}");
        }
    }

    #[test]
    fn child_shell_environment_text_is_inert_when_not_executed() {
        for command in [
            "printf '%s' \"BASH_ENV=/tmp/attacker-startup bash -c 'rm -rf /'\"",
            "printf '%s' \"env BASH_ENV=/tmp/attacker-startup bash -c 'rm -rf /'\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn eval_state_flows_back_to_outer_commands() {
        assert_eq!(
            evaluate_result("eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\""),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn child_shell_state_does_not_escape() {
        assert_eq!(
            evaluate_result("sh -c 'TARGET=/'; printf '%s' \"$TARGET\""),
            SafetyEvaluation::NoDeterministicDecision
        );

        let deny = evaluate_command("sh -c 'TARGET=/'; sh -c 'rm -rf \"$TARGET\"'")
            .expect("unknown child state must fail closed");
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn child_shell_execution_preserves_outer_state() {
        assert_eq!(
            evaluate_result("TARGET=/tmp/safe; sh -c 'printf ok'; rm -rf \"$TARGET\""),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert!(matches!(
            evaluate_result("TARGET=/tmp/safe; sh -c \"$UNKNOWN\"; rm -rf \"$TARGET\""),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn child_shell_only_inherits_an_unchanged_trusted_home() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let control = evaluate_command("sh -c 'rm -rf \"$HOME\"'")
            .expect("unchanged HOME must remain a trusted child target");
        assert_eq!(control.rule_id, "irreversible-home-delete");

        for command in [
            "HOME=/tmp/safe; sh -c 'rm -rf \"$HOME\"'",
            "unset HOME; sh -c 'rm -rf \"$HOME\"'",
            "unknown_command; sh -c 'rm -rf \"$HOME\"'",
            "HOME=/tmp/safe sh -c 'rm -rf \"$HOME\"'",
            "env -i sh -c 'rm -rf \"$HOME\"'",
            "env -u HOME sh -c 'rm -rf \"$HOME\"'",
        ] {
            let deny = evaluate_command(command).expect("untrusted child HOME must fail closed");
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        let selector = evaluate_command("HOME=rm; sh -c '$HOME --no-preserve-root -rf /'")
            .expect("untrusted child HOME command selector must fail closed");
        assert_eq!(selector.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn proven_sibling_deny_dominates_nested_uncertainty() {
        assert!(matches!(
            evaluate_result("eval \"$UNKNOWN\"; rm --no-preserve-root -rf /"),
            SafetyEvaluation::Deny(_)
        ));
    }

    #[test]
    fn quote_fragmented_nested_execution_is_literal() {
        assert_eq!(
            evaluate_result("'sh' -'c' 'printf ok'"),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn undecoded_ansi_c_nested_execution_option_is_indeterminate() {
        assert!(matches!(
            evaluate_result("sh $'-\\x63' 'printf ok'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn nested_execution_positional_parameters_are_not_seeded() {
        let deny = evaluate_command("sh -c '$0 --no-preserve-root -rf /' rm")
            .expect("unresolved positional command selection must deny");
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn nested_execution_option_terminator_disables_command_string_mode() {
        assert!(matches!(
            evaluate_result("sh -- -c 'rm --no-preserve-root -rf /'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn uncertain_nested_execution_options_still_scan_a_located_destructive_payload() {
        for command in [
            "bash -lc 'rm --no-preserve-root -rf /'",
            "bash -l -c 'rm --no-preserve-root -rf /'",
            "bash -ic 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).expect("located destructive payload must deny");
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn uncertain_nested_execution_options_abstain_after_a_benign_payload() {
        for command in [
            "bash -lc 'printf ok'",
            "bash -l -c 'printf ok'",
            "bash -ic 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn invalid_interpreter_clusters_do_not_mislocate_command_strings() {
        for command in [
            "bash -zc 'printf ok'",
            "bash -cz 'rm --no-preserve-root -rf /'",
            "bash -Oc 'rm --no-preserve-root -rf /'",
            "bash -co 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        for command in [
            "bash -lc 'rm --no-preserve-root -rf /'",
            "bash -cl 'rm --no-preserve-root -rf /'",
            "sh -xc 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).expect("supported -c cluster must be scanned");
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn pipeline_and_subshell_eval_use_discarded_state() {
        for command in [
            "TARGET=/tmp/safe; eval 'TARGET=/' | cat; rm -rf \"$TARGET\"",
            "TARGET=/tmp/safe; (eval 'TARGET=/'); rm -rf \"$TARGET\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }

        for command in [
            "eval 'rm --no-preserve-root -rf /' | cat",
            "(eval 'rm --no-preserve-root -rf /')",
        ] {
            let deny = evaluate_command(command).expect("isolated nested danger must be scanned");
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        assert!(matches!(
            evaluate_result("TARGET=/tmp/safe; eval \"$UNKNOWN\" | cat; rm -rf \"$TARGET\""),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn timed_prefix_assignments_reach_eval_and_child_shell_policy() {
        assert_eq!(
            evaluate_result("SAFE=/; time -- SAFE=/tmp eval 'rm -rf \"$SAFE\"'"),
            SafetyEvaluation::NoDeterministicDecision
        );

        let eval_deny = evaluate_command("time -- TARGET=/ eval 'rm -rf \"$TARGET\"'")
            .expect("timed eval prefix assignment must be visible");
        assert!(matches!(
            eval_deny.rule_id,
            "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
        ));

        assert!(matches!(
            evaluate_result("time -- BASH_ENV=/tmp/startup sh -c 'printf ok'"),
            SafetyEvaluation::Indeterminate(_)
        ));
        let child_deny =
            evaluate_command("time -- BASH_ENV=/tmp/startup sh -c 'rm --no-preserve-root -rf /'")
                .expect("timed child metadata must not hide destruction");
        assert_eq!(child_deny.rule_id, "irreversible-root-delete");
    }

    #[test]
    fn unknown_and_value_taking_nested_execution_options_are_indeterminate() {
        for command in [
            "bash -z -c 'printf ok'",
            "bash -o posix -c 'printf ok'",
            "bash -O extglob -c 'printf ok'",
            "bash --rcfile profile -c 'printf ok'",
            "bash --norc -c 'printf ok'",
            "bash --rcf profile -c 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn multicall_nested_execution_requires_an_exact_applet_selector() {
        for command in [
            "busybox",
            "APPLET=sh; busybox \"$APPLET\" -c 'printf ok'",
            "busybox ls",
            "busybox --list",
            "toybox",
            "APPLET=sh; toybox \"$APPLET\" -c 'printf ok'",
            "toybox printf ok",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn multicall_command_carrying_applets_preserve_benign_literal_programs() {
        for command in [
            "busybox env sh -c 'printf ok'",
            "busybox time sh -c 'printf ok'",
            "toybox env sh -c 'printf ok'",
            "toybox time sh -c 'printf ok'",
            "busybox env toybox time sh -c 'printf ok'",
            "printf '%s' \"busybox env sh -c 'rm -rf /'\"",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn ambiguous_time_options_are_indeterminate() {
        for command in [
            "/usr/bin/time --unknown sh -c 'printf ok'",
            "/usr/bin/time --out log sh -c 'printf ok'",
            "/usr/bin/time --help sh -c 'printf ok'",
            "/usr/bin/time -o",
            "busybox time --unknown sh -c 'printf ok'",
            "busybox time -f",
            "toybox time -Z sh -c 'printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        assert_eq!(
            evaluate_result("time -h sh -c 'rm --no-preserve-root -rf /'"),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn direct_external_wrapper_nonexecuting_forms_are_indeterminate() {
        for command in [
            "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time --verbose sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -vf FORMAT sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -vo log sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -volog sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -vfFORMAT sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env --ver sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -i0 sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -0i sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -a displayed sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env --argv0=displayed sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -iC/tmp sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env --chdir=/tmp sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn env_assignments_end_option_parsing() {
        for command in [
            "env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'",
            "env FOO=bar -- sh -c 'rm --no-preserve-root -rf /'",
            "env FOO=bar BAR=baz -v sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        for command in [
            "env -i FOO=bar sh -c 'rm --no-preserve-root -rf /'",
            "env -v FOO=bar BAR=baz sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn dynamic_direct_wrapper_transitions_remain_indeterminate() {
        for command in [
            "/usr/bin/time \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
            "env \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
            "env FOO=bar \"$COMMAND\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn direct_wrapper_common_option_arity_reaches_the_child() {
        for command in [
            "/usr/bin/time -o log sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -olog sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -aolog sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u HOME sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -uHOME sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -ivuHOME sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn invalid_or_unknown_wrapper_option_values_are_indeterminate() {
        for command in [
            "/usr/bin/time -o '' sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -o '' sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u '' sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -uA=B sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
            "builtin command /usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "builtin exec /usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}: {:?}",
                evaluate_result(command)
            );
        }
    }

    #[test]
    fn valid_wrapper_option_values_reach_destructive_children() {
        for command in [
            "/usr/bin/time -o log sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -aolog sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -olog sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -vfFORMAT sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -f '' sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u HOME sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -ivuHOME sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u '' sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -uA=B sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn invalid_wrapper_value_keeps_deny_precedence_in_both_orders() {
        for command in [
            "/usr/bin/time -o '' sh -c 'rm -rf /'; rm --no-preserve-root -rf /",
            "rm --no-preserve-root -rf /; /usr/bin/env -u '=HOME' sh -c 'rm -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn attached_dynamic_wrapper_values_preserve_value_semantics() {
        for command in [
            "busybox time -o\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -f\"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -oX\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -uX\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox time -oX\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -iu\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}: {:?}",
                evaluate_result(command)
            );
        }

        for command in [
            "busybox time -vfX\"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -uX\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u\"$NAME\"X sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -iu\"$NAME\"X sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        for command in [
            "busybox env -u\"$@\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -uX\"$@\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u\"$@\"X sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -uX\"${VALUES[@]}\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -u\"${VALUES[@]}\"X sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn dynamic_attached_non_value_options_do_not_project_children() {
        for command in [
            "/usr/bin/time -a\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -i\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}: {:?}",
                evaluate_result(command)
            );
        }

        for command in [
            "busybox time -a\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -i\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn expanding_separate_wrapper_option_values_fail_closed() {
        for command in [
            "VALUE='log sh'; /usr/bin/time -o $VALUE 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -o {log,sh} 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -o * 'rm --no-preserve-root -rf /'",
            "VALUE='HOME sh'; /usr/bin/env -u $VALUE 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u {HOME,sh} 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u * 'rm --no-preserve-root -rf /'",
            "VALUE='log sh'; busybox time -o $VALUE 'rm --no-preserve-root -rf /'",
            "busybox time -o {log,sh} 'rm --no-preserve-root -rf /'",
            "busybox time -o * 'rm --no-preserve-root -rf /'",
            "VALUE='HOME sh'; busybox env -u $VALUE 'rm --no-preserve-root -rf /'",
            "busybox env -u {HOME,sh} 'rm --no-preserve-root -rf /'",
            "busybox env -u * 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn quoted_multi_argv_separate_wrapper_values_fail_closed() {
        for command in [
            "/usr/bin/time -o \"$@\" 'rm --no-preserve-root -rf /'",
            "/usr/bin/env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
            "busybox time -o \"$@\" 'rm --no-preserve-root -rf /'",
            "busybox time -f \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
            "busybox env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn nested_builtin_quoted_multi_argv_values_remain_deterministic_denies() {
        for command in [
            "builtin command /usr/bin/time -o \"$@\" 'rm --no-preserve-root -rf /'",
            "builtin exec /usr/bin/env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
            "builtin command busybox time -f \"$@\" 'rm --no-preserve-root -rf /'",
            "builtin exec busybox env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn quoted_multi_argv_fallback_option_values_remain_deterministic_denies() {
        for command in [
            "/usr/bin/time -o \"${VALUE:-$@}\" 'rm --no-preserve-root -rf /'",
            "busybox env -u \"${VALUE:-${VALUES[@]}}\" 'rm --no-preserve-root -rf /'",
            "builtin command busybox time -f \"${VALUE:+$@}\" 'rm --no-preserve-root -rf /'",
            "/usr/bin/time -o ${VALUE:-$LOG} 'rm --no-preserve-root -rf /'",
        ] {
            let deny = evaluate_command(command)
                .unwrap_or_else(|| panic!("{command}: {:?}", evaluate_result(command)));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn dynamic_separate_wrapper_option_values_are_indeterminate() {
        for command in [
            "VALUE='log sh'; /usr/bin/time -o \"$VALUE\" printf ok",
            "VALUE='HOME sh'; /usr/bin/env -u \"$VALUE\" printf ok",
            "VALUE='log sh'; busybox time -o \"$VALUE\" printf ok",
            "/usr/bin/time -o \"${VALUE:-$LOG}\" printf ok",
            "/usr/bin/env -u \"${VALUE:+$*}\" printf ok",
            "builtin command /usr/bin/time -o \"${VALUE:-$LOG}\" printf ok",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}: {:?}",
                evaluate_result(command)
            );
        }
    }

    #[test]
    fn literal_or_busybox_env_separate_wrapper_option_values_do_not_deny() {
        for command in [
            "/usr/bin/time -o log printf ok",
            "/usr/bin/env -u HOME printf ok",
            "busybox time -o log printf ok",
            "busybox time -o $'log' printf ok",
            "busybox time -f \"$VALUE\" printf ok",
            "busybox time -f \"${VALUE:-${FORMAT[*]}}\" printf ok",
            "builtin exec busybox time -f \"${VALUE:+$*}\" printf ok",
            "VALUE='HOME sh'; busybox env -u \"$VALUE\" printf ok",
            "busybox env -u HOME printf ok",
            "busybox env -u ~ printf ok",
            "busybox env -u \"${VALUE:-${NAME:-HOME}}\" printf ok",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn uncertain_direct_wrapper_keeps_deny_precedence_in_both_orders() {
        for command in [
            "/usr/bin/env --help sh -c 'rm -rf /'; rm --no-preserve-root -rf /",
            "rm --no-preserve-root -rf /; /usr/bin/env --help sh -c 'rm -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn multicall_terminating_or_unsupported_options_are_indeterminate() {
        for command in [
            "busybox time -h sh -c 'rm --no-preserve-root -rf /'",
            "busybox env --help sh -c 'rm --no-preserve-root -rf /'",
            "busybox env -v sh -c 'rm --no-preserve-root -rf /'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn nested_execution_result_precedence_is_order_independent() {
        for command in [
            "sh -c \"$PROGRAM\"; rm --no-preserve-root -rf /",
            "rm --no-preserve-root -rf /; sh -c \"$PROGRAM\"",
        ] {
            assert!(matches!(
                evaluate_result(command),
                SafetyEvaluation::Deny(_)
            ));
        }
        for command in [
            "sh -c \"$PROGRAM\"; printf ok",
            "sh -c 'bash script.sh; printf ok'",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn eval_state_flows_into_and_out_of_top_level_eval() {
        for command in [
            "TARGET=/; eval 'rm --no-preserve-root -rf \"$TARGET\"'",
            "eval 'TARGET=/'; rm --no-preserve-root -rf \"$TARGET\"",
        ] {
            assert!(matches!(
                evaluate_result(command),
                SafetyEvaluation::Deny(_)
            ));
        }
    }

    #[test]
    fn indeterminate_eval_invalidates_caller_state_before_siblings() {
        let deny = evaluate_command(
            "TARGET=/tmp/safe; eval \"$UNKNOWN\"; rm --no-preserve-root -rf \"$TARGET\"",
        )
        .expect("unknown eval effects must invalidate the known-safe target");
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn eval_state_in_pipeline_and_subshell_does_not_escape() {
        for command in [
            "TARGET=/; eval 'TARGET=/tmp/safe' | cat; rm -rf \"$TARGET\"",
            "TARGET=/; (eval 'TARGET=/tmp/safe'); rm -rf \"$TARGET\"",
        ] {
            assert!(matches!(
                evaluate_result(command),
                SafetyEvaluation::Deny(_)
            ));
        }

        for command in [
            "eval 'TARGET=/' | cat; eval 'rm -rf \"$TARGET\"'",
            "eval 'TARGET=/' & eval 'rm -rf \"$TARGET\"'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn empty_eval_arguments_preserve_the_joined_program_positions() {
        assert_eq!(
            evaluate_result("eval '' 'printf ok' ''"),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert_eq!(
            evaluate_result("eval"),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn malformed_nested_program_is_indeterminate() {
        assert!(matches!(
            evaluate_result("sh -c 'if'"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    fn shell_single_quote(source: &str) -> String {
        format!("'{}'", source.replace('\'', "'\\''"))
    }

    fn nested_eval(depth: usize, leaf: &str) -> String {
        (0..depth).fold(leaf.to_string(), |source, _| {
            format!("eval {}", shell_single_quote(&source))
        })
    }

    #[test]
    fn recursive_parse_byte_budget_allows_exact_limit_and_rejects_plus_one() {
        let payload = "x".repeat((MAX_SHELL_COMMAND_BYTES - 8) / 2);
        let exact = format!("sh -c '{payload}'");
        assert_eq!(exact.len() + payload.len(), MAX_SHELL_COMMAND_BYTES);
        assert_eq!(
            evaluate_result(&exact),
            SafetyEvaluation::NoDeterministicDecision
        );

        let plus_one = format!("{exact} ");
        assert_eq!(plus_one.len() + payload.len(), MAX_SHELL_COMMAND_BYTES + 1);
        assert_eq!(
            evaluate_result(&plus_one),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit)
        );
    }

    #[test]
    fn recursive_parse_byte_budget_accumulates_across_sibling_payloads() {
        let source = std::iter::repeat_n(format!("sh -c '{}'", "x".repeat(100)), 313)
            .collect::<Vec<_>>()
            .join("; ");
        assert!(source.len() < MAX_SHELL_COMMAND_BYTES);
        assert!(
            source.len() + 313 * 100 > MAX_SHELL_COMMAND_BYTES,
            "fixture must exhaust only the cumulative parse budget"
        );
        assert_eq!(
            evaluate_result(&source),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit)
        );
    }

    #[test]
    fn nested_execution_depth_budget_allows_exact_limit_and_rejects_plus_one() {
        const NESTED_LIMIT: usize = 8;
        assert_eq!(
            evaluate_result(&nested_eval(NESTED_LIMIT, ":")),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert_eq!(
            evaluate_result(&nested_eval(NESTED_LIMIT + 1, ":")),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit)
        );
    }

    #[test]
    fn nested_execution_depth_budget_is_restored_between_siblings() {
        let nested = nested_eval(8, ":");
        assert_eq!(
            evaluate_result(&format!("{nested}; {nested}")),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn deferred_builtin_actions_share_recursive_depth_and_byte_budgets() {
        for source in ["trap ':' EXIT", "mapfile -c1 -C ':'"] {
            assert_eq!(
                evaluate_with_limits(source, usize::MAX, 0, usize::MAX),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit),
                "{source}"
            );
            assert_eq!(
                evaluate_with_limits(source, source.len(), 8, usize::MAX),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit),
                "{source}"
            );
        }
    }

    fn minimum_analysis_node_limit(source: &str) -> usize {
        (0..1_024)
            .find(|limit| {
                shell::analyze_with_budget(source, &mut shell::AnalysisBudget::with_limit(*limit))
                    .is_ok()
            })
            .expect("small fixture must fit within the probe range")
    }

    fn evaluate_with_limits(
        source: &str,
        remaining_bytes: usize,
        remaining_nested: usize,
        node_limit: usize,
    ) -> SafetyEvaluation {
        evaluate_program(
            source,
            &mut EvaluationState::trusted(),
            &mut EvaluationBudget::with_limits(
                remaining_bytes,
                remaining_nested,
                shell::AnalysisBudget::with_limit(node_limit),
            ),
            ShellSemantics::Bash,
        )
    }

    #[test]
    fn recursive_analysis_node_budget_is_cumulative_and_exact() {
        let source = "sh -c ':'; sh -c ':'";
        let exact_limit =
            minimum_analysis_node_limit(source) + 2 * minimum_analysis_node_limit(":");

        assert_eq!(
            evaluate_with_limits(source, usize::MAX, 8, exact_limit),
            SafetyEvaluation::NoDeterministicDecision
        );
        assert_eq!(
            evaluate_with_limits(source, usize::MAX, 8, exact_limit - 1),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit)
        );
    }

    #[test]
    fn recursive_pattern_budget_is_shared_and_fails_closed() {
        let nested = "sh -c 'IFS=:; X='\\''/xa99z*/**/**/**/zz'\\''; rm -rf $X'";
        let mut state = EvaluationState::trusted();
        let mut probe = EvaluationBudget::new();
        assert_eq!(
            evaluate_program(nested, &mut state, &mut probe, ShellSemantics::Bash),
            SafetyEvaluation::NoDeterministicDecision
        );
        let consumed = MAX_PATTERN_MATCH_STATES - probe.patterns.remaining_states;
        assert!(consumed > 0);

        let source = format!("{nested}; {nested}");
        let mut budget = EvaluationBudget::new();
        budget.patterns.remaining_states = consumed * 2 - 1;
        let deny = evaluate_program(
            &source,
            &mut EvaluationState::trusted(),
            &mut budget,
            ShellSemantics::Bash,
        );
        assert!(matches!(
            deny,
            SafetyEvaluation::Deny(SafetyDeny {
                rule_id: "unsafe-recursive-delete-expansion",
                ..
            })
        ));
    }

    #[test]
    fn reopened_parser_backed_policy_corpus() {
        let mut mismatches = Vec::new();
        for (command, expected_rule_id) in [
            (
                "/bin/r[]m] --no-preserve-root -rf /",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "/bin/r[\\m] --no-preserve-root -rf /",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "/bin/r[m] --no-preserve-root -rf /",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "rm>/dev/null --no-preserve-root -rf /",
                "irreversible-root-delete",
            ),
            (
                ">'>' rm --no-preserve-root -rf /",
                "irreversible-root-delete",
            ),
            (
                "rm --no-preserve-root -rf /{,}",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "FLAGS=-rf; rm $FLAGS /",
                "unsafe-recursive-delete-expansion",
            ),
            ("rm -rf $'\\x2f'", "unsafe-recursive-delete-expansion"),
            (
                "cat <(rm --no-preserve-root -rf /)",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "{ rm --no-preserve-root -rf /; }",
                "unsafe-recursive-delete-expansion",
            ),
        ] {
            match evaluate_command(command) {
                Some(deny) if deny.rule_id == expected_rule_id => {}
                Some(deny) => mismatches.push(format!(
                    "{command:?}: expected {expected_rule_id}, got {}",
                    deny.rule_id
                )),
                None => mismatches.push(format!(
                    "{command:?}: expected {expected_rule_id}, got no decision"
                )),
            }
        }

        for command in [
            "\"2\">/dev/null rm -rf /",
            "printf %s foo{bar}",
            "((0 || rm -rf / 1))",
            "[;]",
        ] {
            if let Some(deny) = evaluate_command(command) {
                mismatches.push(format!(
                    "{command:?}: expected no decision, got {}",
                    deny.rule_id
                ));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
        assert_eq!(
            evaluate_result("env $'-\\x53' 'rm -rf /'"),
            SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
        );
    }

    #[test]
    fn irreversible_root_delete_denies() {
        for command in [
            "rm -rf /",
            "/bin/rm -rf /",
            "/bin/rm -R /",
            "rm --no-preserve-root -Rf /./",
            "rm -rf ///",
            "rm -rf //./",
            "echo ready | rm -rf /",
            "sudo rm -rf -- /",
            "sudo -n /usr/bin/rm -rf /",
            "sudo --user root /usr/bin/rm -rf /",
            "busybox env -- /bin/rm -rf /",
            "env -- /bin/rm -rf /",
            "env -u PATH /bin/rm -rf /",
            "$'rm' --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }
    }

    #[test]
    fn supported_wrappers_preserve_literal_and_dynamic_command_selection() {
        for command in [
            "sudo --user=root rm -rf /",
            "sudo --u root rm -rf /",
            "sudo -uroot rm -rf /",
            "sudo -nu root rm -rf /",
            "sudo FOO=bar -- rm -rf /",
            "sudo 'FOO=bar' rm -rf /",
            "env 'FOO=bar' rm -rf /",
            "env - rm -rf /",
            "exec -a displayed rm -rf /",
            "exec -a NAME=foo rm -rf /",
            "command -- rm -rf /",
            "time -p rm -rf /",
            "/usr/bin/time -o log rm -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        for command in [
            "env FOO=bar -- rm -rf /",
            "env -iC/tmp rm -rf /",
            "env --chdir=/tmp rm -rf /",
            "/usr/bin/time -vo log rm -rf /",
            "/usr/bin/time -vf FORMAT rm -rf /",
            "/usr/bin/time -volog rm -rf /",
            "/usr/bin/time -vfFORMAT rm -rf /",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }

        for command in [
            "OPTIONS=--user=root; sudo $OPTIONS rm -rf /",
            "OPTIONS=-i; env $OPTIONS rm -rf /",
            "OPTIONS=-a; exec $OPTIONS displayed rm -rf /",
            "OPTIONS=-p; command $OPTIONS rm -rf /",
            "OPTIONS=-p; time $OPTIONS rm -rf /",
            "sudo FOO=$VALUE rm -rf /",
            "env FOO=$VALUE rm -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn control_flow_assignments_do_not_become_definite_top_level_state() {
        for command in [
            "ROOT=/; if false; then ROOT=/tmp; fi; rm -rf \"$ROOT\"",
            "ROOT=/; while false; do ROOT=/tmp; done; rm -rf \"$ROOT\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        let pipeline = "ROOT=/; ROOT=/tmp | cat; rm -rf \"$ROOT\"";
        let deny = evaluate_command(pipeline).unwrap_or_else(|| panic!("{pipeline}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{pipeline}"
        );

        let subshell = "ROOT=/; (ROOT=/tmp); rm -rf \"$ROOT\"";
        let deny = evaluate_command(subshell).unwrap_or_else(|| panic!("{subshell}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{subshell}"
        );
    }

    #[test]
    fn command_local_assignments_use_caller_state_then_invalidate_it() {
        for command in [
            "ROOT=/; ROOT=/tmp rm -rf \"$ROOT\"",
            "ROOT=/; ROOT=/tmp true; rm -rf \"$ROOT\"",
            "ROOT=/tmp; ROOT=/ true; rm -rf \"$ROOT\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn conditional_background_and_for_assignments_cannot_make_state_safe() {
        for command in [
            "ROOT=/; false && ROOT=/tmp; rm -rf \"$ROOT\"",
            "ROOT=/; ROOT=/tmp & rm -rf \"$ROOT\"",
            "ROOT=/tmp; for ROOT in /tmp /; do true; done; rm -rf \"$ROOT\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn dynamic_flag_ambiguity_respects_known_targets_and_option_terminator() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for command in [
            "rm -rf $UNKNOWN /",
            "FLAG=-rf; rm -f $FLAG /",
            "HOME=-rf; rm -f $HOME /",
            "rm -rf -- $UNKNOWN /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        let home = evaluate_command("rm -rf $HOME").expect("known home target");
        assert_eq!(home.rule_id, "irreversible-home-delete");

        for command in [
            "TMPDIR=; rm -rf \"${TMPDIR:-/tmp}/work\"",
            "SAFE=target; rm -f $SAFE /",
            "FLAG=-rf; rm -f -- $FLAG /",
            "rm -f -- $UNKNOWN /",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn append_assignments_preserve_destructive_values() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for command in [
            "X=-; X+=rf; rm --no-preserve-root -f $X /",
            "X=-; X+=; X+=r; X+=f; rm --no-preserve-root -f $X /",
            "X+=-rf; rm --no-preserve-root -f $X /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let split = home
            .char_indices()
            .next_back()
            .expect("HOME must not be empty")
            .0;
        let command = format!(
            "X='{}'; X+='{}'; rm -rf \"$X\"",
            &home[..split],
            &home[split..]
        );
        let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
    }

    #[test]
    fn home_alias_classification_respects_field_splitting() {
        let output = std::process::Command::new(
            std::env::current_exe().expect("test executable must be available"),
        )
        .args([
            "--ignored",
            "--exact",
            "brain::safety::tests::home_alias_classification_field_splitting_subprocess_helper",
            "--nocapture",
        ])
        .env("CODING_BRAIN_TJNX_SUBPROCESS", "1")
        .output()
        .expect("field-splitting subprocess must run");
        assert!(
            output.status.success(),
            "field-splitting subprocess failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn home_alias_classification_field_splitting_subprocess_helper() {
        if std::env::var_os("CODING_BRAIN_TJNX_SUBPROCESS").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = tempfile::tempdir().expect("test fixture root must be created");
        let home = fixture.path().join("home").join("developer");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).expect("test fixture cwd must be created");
        let _context = ProcessContextGuard::set(&home, &cwd);
        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let home_path = Path::new(&home);
        let home_parent = home_path.parent().expect("HOME must have a parent");
        let home_name = home_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("HOME must end with a UTF-8 component");
        let home_parent_name = home_parent
            .file_name()
            .and_then(|name| name.to_str())
            .expect("HOME parent must end with a UTF-8 component");
        let descendant = home_path.join("safe");
        let sibling = home_parent.join("xa99-sibling");
        let home_root_component = home_path
            .components()
            .find_map(|component| match component {
                Component::Normal(part) => part.to_str(),
                _ => None,
            })
            .expect("HOME must start with a UTF-8 component");
        let root_pattern = format!(
            "{}*",
            home_root_component
                .chars()
                .next()
                .expect("HOME component must not be empty")
        );
        let unrelated_pattern = unrelated_pattern_prefix(home_path);
        let repeated_home_pattern =
            format!("//{}*", home.trim_start_matches('/').replace('/', "//"));
        let mut globstar_home_parts = home
            .trim_start_matches('/')
            .split('/')
            .map(str::to_string)
            .collect::<Vec<_>>();
        globstar_home_parts
            .last_mut()
            .expect("HOME must contain a component")
            .push('*');
        let globstar_home_pattern = format!("/**/**/{}", globstar_home_parts.join("/"));
        let globstar_ancestor_pattern = format!("/**/**/{home_root_component}");
        let nocase_home_pattern = format!("{}*", home.to_uppercase());
        let split = home
            .char_indices()
            .next_back()
            .expect("HOME must not be empty")
            .0;
        let assignment = format!("X='{}'; X+='{}'", &home[..split], &home[split..]);

        let split_alias = format!("IFS=/; {assignment}; rm -rf $X");
        let deny = evaluate_command(&split_alias).unwrap_or_else(|| panic!("{split_alias}"));
        assert_eq!(deny.rule_id, "irreversible-home-delete", "{split_alias}");

        for command in [
            format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
            format!("IFS=/; X='{home}'; X+=/safe; rm -rf $X"),
            format!("IFS=/; X='{}'; rm -rf $X", sibling.display()),
            format!("IFS=/; X='/{root_pattern}/safe'; rm -rf $X"),
            format!("IFS=/; X='{root_pattern}'; rm -rf $X"),
            format!("IFS=,; X='./{home_name}*'; rm -rf $X"),
        ] {
            let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
        }

        let parent_traversal = "IFS=,; X='../safe'; rm -rf $X";
        let deny =
            evaluate_command(parent_traversal).unwrap_or_else(|| panic!("{parent_traversal}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{parent_traversal}"
        );

        let repeated_separator_pattern = format!("IFS=,; X='{repeated_home_pattern}'; rm -rf $X");
        let deny = evaluate_command(&repeated_separator_pattern)
            .unwrap_or_else(|| panic!("{repeated_separator_pattern}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{repeated_separator_pattern}"
        );
        for command in [
            format!("shopt -s globstar; IFS=:; X='{globstar_home_pattern}'; rm -rf $X"),
            format!("shopt -s nocaseglob; IFS=:; X='{nocase_home_pattern}'; rm -rf $X"),
        ] {
            let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
        let globstar_ancestor =
            format!("shopt -s globstar; IFS=:; X='{globstar_ancestor_pattern}'; rm -rf $X");
        let deny =
            evaluate_command(&globstar_ancestor).unwrap_or_else(|| panic!("{globstar_ancestor}"));
        assert_eq!(
            deny.rule_id, "irreversible-home-delete",
            "{globstar_ancestor}"
        );

        let globstar_parent_zero =
            format!("shopt -s globstar; IFS=:; X='{home}/**/../{home_name}'; rm -rf $X");
        let globstar_parent_nonzero = format!(
            "shopt -s globstar; IFS=:; X='{}/**/../{home_name}'; rm -rf $X",
            home_parent.display()
        );
        let globstar_parent_chained = format!(
            "shopt -s globstar; IFS=:; X='{home}/**/../../{home_parent_name}/{home_name}'; rm -rf $X"
        );
        let wildcard_parent = format!(
            "IFS=:; X='{}/{home_name}*/../{home_name}'; rm -rf $X",
            home_parent.display()
        );

        let confirmed_bypass = evaluate_command(&globstar_parent_zero)
            .unwrap_or_else(|| panic!("{globstar_parent_zero}"));
        assert_eq!(
            confirmed_bypass.rule_id, "irreversible-home-delete",
            "{globstar_parent_zero}"
        );
        for command in [
            globstar_parent_nonzero,
            globstar_parent_chained,
            wildcard_parent,
        ] {
            assert!(evaluate_command(&command).is_some(), "{command}");
        }

        let quoted_globstar_parent = format!("X='{home}/**/../{home_name}'; rm -rf \"$X\"");
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

        for command in [
            "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/..'; rm -rf $X",
            "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/**/..'; rm -rf $X",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
        }

        let conservative_direct_root =
            "shopt -s globstar; IFS=:; X='/**/xa99-unrelated/**'; rm -rf $X";
        let deny = evaluate_command(conservative_direct_root)
            .unwrap_or_else(|| panic!("{conservative_direct_root}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{conservative_direct_root}"
        );

        for command in [
            format!("{assignment}; rm -rf \"$X\""),
            "rm -rf $HOME".to_string(),
        ] {
            let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
        }

        for command in [
            format!("X='{}'; rm -rf \"$X\"", descendant.display()),
            "IFS=/; X=/xa99-safe-control/target; rm -rf $X".to_string(),
            format!("IFS=/; X='/{unrelated_pattern}*/safe'; rm -rf $X"),
            format!("IFS=/; X='{unrelated_pattern}*'; rm -rf $X"),
            format!("IFS=:; X='{home_root_component}*/safe'; rm -rf $X"),
        ] {
            assert!(evaluate_command(&command).is_none(), "{command}");
        }
    }

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
            pattern_components_may_normalize_to(&patterns, &["safe"], true, &mut state_budget),
            PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal)
        );
        assert_eq!(state_budget.remaining_states, 1);
        assert_eq!(
            pattern_components_may_normalize_to(&patterns, &["safe"], true, &mut state_budget),
            PatternReachability::Unknown
        );
        assert_eq!(state_budget.remaining_states, 0);

        let mut component_budget = PatternMatchBudget {
            remaining_states: MAX_PATTERN_MATCH_STATES,
            remaining_components: 4,
        };
        assert_eq!(
            pattern_components_may_normalize_to(&patterns, &["safe"], true, &mut component_budget),
            PatternReachability::Reachable(PatternMatchKind::ExpansionThenTraversal)
        );
        assert_eq!(component_budget.remaining_components, 0);
        assert_eq!(
            pattern_components_may_normalize_to(&patterns, &["safe"], true, &mut component_budget),
            PatternReachability::Unknown
        );

        let safe_pattern = "/xa99z*/**/**/**/zz";
        let safe_command = format!("IFS=:; X='{safe_pattern}'; rm -rf $X");
        assert!(evaluate_command(&safe_command).is_none(), "{safe_command}");

        let fields = std::iter::repeat_n(safe_pattern, 500)
            .collect::<Vec<_>>()
            .join(":");
        let exhausting_command = format!("IFS=:; X='{fields}'; rm -rf $X");
        let deny = evaluate_command(&exhausting_command)
            .unwrap_or_else(|| panic!("aggregate pattern budget was not exhausted"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "aggregate pattern budget exhaustion must fail closed"
        );
    }

    #[test]
    fn unquoted_parameter_field_splitting_cannot_hide_recursive_flags_or_root_targets() {
        for command in [
            "X='safe -rf'; rm -f $X /",
            "X='safe /'; rm -rf $X",
            "IFS=e; X=safe-rf; rm -f $X /",
            "IFS=,; X='safe,/'; rm -rf $X",
            "SEP=e; IFS=$SEP; X=safe-rf; rm -f $X /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        for command in [
            "IFS=; X='safe -rf'; rm -f $X /",
            "IFS=e; X=saf; rm -f $X-e-rf /",
            "IFS=e; X=safe-rf; rm -f \"$X\" /",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn tilde_expansion_uses_tracked_shell_variables_when_classifying_flags() {
        for command in ["HOME=-rf; rm -f ~ /", "PWD=-Rf; rm -f ~+ /"] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn commands_not_proven_state_preserving_invalidate_tracked_assignments() {
        for command in [
            "ROOT=/tmp; export ROOT=/; rm -rf \"$ROOT\"",
            "ROOT=/tmp; readonly ROOT=/; rm -rf \"$ROOT\"",
            "ROOT=/tmp; arbitrary_mutator; rm -rf \"$ROOT\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn unknown_commands_do_not_restore_trusted_home_state() {
        for command in [
            "arbitrary_mutator; rm -f \"$HOME\" /",
            "HOME=/home/alexander; arbitrary_mutator; rm -f \"$HOME\" /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn invalidated_home_never_falls_back_to_the_initial_policy_context() {
        for command in [
            "HOME=/tmp; export HOME=-rf; rm -f \"$HOME\" /",
            "HOME=/tmp; export HOME=-rf; rm -f ~ /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn printf_percent_n_invalidates_tracked_assignments() {
        let command = "X=; printf %n X; rm -f \"${X:+-rf}\" /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn arithmetic_and_lastpipe_parent_mutation_invalidate_tracked_assignments() {
        for command in [
            "X=; ((X=1)); rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; if false; then shopt -u lastpipe; fi; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn external_shopt_cannot_enable_lastpipe_in_the_caller() {
        for command in [
            "env shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "sudo shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "/usr/bin/time shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "exec shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::NoDeterministicDecision,
                "{command}"
            );
        }
    }

    #[test]
    fn external_shopt_cannot_disable_lastpipe_in_the_caller() {
        for command in [
            "shopt -s lastpipe; env shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; sudo shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; /usr/bin/time shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; exec shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn child_shopt_updates_only_the_child_lastpipe_state() {
        let command = "bash -c 'shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /'; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /";
        let deny = evaluate_command(command).expect("child lastpipe mutation must be analyzed");

        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
        assert_eq!(
            evaluate_result(
                "bash -c 'shopt -s lastpipe'; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /"
            ),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn direct_shopt_can_disable_lastpipe_in_the_caller() {
        assert_eq!(
            evaluate_result(
                "shopt -s lastpipe; shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /"
            ),
            SafetyEvaluation::NoDeterministicDecision
        );
    }

    #[test]
    fn exported_bashopts_can_enable_lastpipe_in_a_child_bash() {
        let command = "shopt -s lastpipe; export BASHOPTS; bash -c 'TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"'";
        let deny = evaluate_command(command)
            .expect("exported BASHOPTS may enable child lastpipe mutation");
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn exported_bash_env_keeps_a_later_child_shell_indeterminate() {
        let command = "BASH_ENV=/dev/fd/3; export BASH_ENV; bash -c 'rm -rf /tmp/safe' 3<<<'rm(){ printf OVERRIDDEN; }'";
        assert!(matches!(
            evaluate_result(command),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn assignment_arithmetic_side_effect_invalidates_tracked_assignments() {
        let command = "X=; Y=$((X=1)); rm -f \"${X:+-rf}\" /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn nested_parameter_arithmetic_in_assignment_invalidates_tracked_assignments() {
        let command = "X=; VALUE=abc; Y=\"${OUTER:-${VALUE:X=1}}\"; rm -f \"${X:+-rf}\" /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn unquoted_parameter_pathname_expansion_can_reach_the_trusted_home() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let last = home
            .char_indices()
            .next_back()
            .expect("HOME must not be empty")
            .0;
        let pattern = format!("{}?", &home[..last]);

        let unquoted = format!("X='{pattern}'; rm -rf $X");
        let deny = evaluate_command(&unquoted).unwrap_or_else(|| panic!("{unquoted}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");

        for command in [
            format!("X='{pattern}'; rm -rf \"$X\""),
            format!("X='{pattern}'; rm -rf $X-suffix"),
        ] {
            assert!(evaluate_command(&command).is_none(), "{command}");
        }
    }

    #[test]
    fn resolved_fields_classify_patterns_after_complete_concatenation() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let last = home
            .char_indices()
            .next_back()
            .expect("HOME must not be empty")
            .0;
        let prefix = &home[..last];
        let final_character = &home[last..];
        let nonmatching_character = if final_character == "x" { "y" } else { "x" };
        let assignments = HashMap::from([
            ("PREFIX".into(), format!("{prefix}[")),
            ("NEGATED_PREFIX".into(), format!("{prefix}[!")),
        ]);

        for (target_syntax, expected_value, expected_pattern) in [
            (
                format!("${{PREFIX}}{final_character}]"),
                format!("{prefix}[{final_character}]"),
                true,
            ),
            (
                format!("${{PREFIX}}\"{final_character}]\""),
                format!("{prefix}[{final_character}]"),
                false,
            ),
            (
                format!("${{PREFIX}}{final_character}\\]"),
                format!("{prefix}[{final_character}]"),
                false,
            ),
            (
                format!("${{PREFIX}}{nonmatching_character}]"),
                format!("{prefix}[{nonmatching_character}]"),
                true,
            ),
            (
                format!("${{NEGATED_PREFIX}}{nonmatching_character}]"),
                format!("{prefix}[!{nonmatching_character}]"),
                true,
            ),
            (
                format!("${{PREFIX}}]{final_character}]"),
                format!("{prefix}[]{final_character}]"),
                true,
            ),
        ] {
            let program = shell::analyze(&format!("rm -rf {target_syntax}")).expect(&target_syntax);
            let target = program.commands[0].arguments.last().expect(&target_syntax);
            let fields = resolve_word_fields(target, &assignments, false).expect(&target_syntax);

            assert_eq!(fields.len(), 1, "{target_syntax}");
            assert_eq!(fields[0].value, expected_value, "{target_syntax}");
            assert_eq!(
                fields[0].parameter_pathname_pattern, expected_pattern,
                "{target_syntax}"
            );
        }
    }

    #[test]
    fn adjacent_unquoted_pathname_pattern_can_complete_the_trusted_home() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let last = home
            .char_indices()
            .next_back()
            .expect("HOME must not be empty")
            .0;
        let prefix = &home[..last];
        let final_character = &home[last..];
        let nonmatching_character = if final_character == "x" { "y" } else { "x" };

        for command in [
            format!("X='{prefix}['; rm -rf ${{X}}{final_character}]"),
            format!("X='{prefix}[!'; rm -rf ${{X}}{nonmatching_character}]"),
            format!("X='{prefix}['; rm -rf ${{X}}]{final_character}]"),
        ] {
            let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        for command in [
            format!("X='{prefix}['; rm -rf ${{X}}\"{final_character}]\""),
            format!("X='{prefix}['; rm -rf ${{X}}{final_character}\\]"),
            format!("X='{prefix}['; rm -rf ${{X}}{final_character}]-suffix"),
        ] {
            assert!(evaluate_command(&command).is_none(), "{command}");
        }
    }

    #[test]
    fn normalized_unquoted_parameter_pathname_expansion_can_reach_the_trusted_home() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
        let home = Path::new(&home);
        let name = home
            .file_name()
            .and_then(|name| name.to_str())
            .expect("HOME must have a UTF-8 final component");
        let last = name
            .char_indices()
            .next_back()
            .expect("HOME final component must not be empty")
            .0;
        let pattern = home
            .join("..")
            .join(format!("{}?", &name[..last]))
            .display()
            .to_string();

        let unquoted = format!("X='{pattern}'; rm -rf $X");
        let deny = evaluate_command(&unquoted).unwrap_or_else(|| panic!("{unquoted}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");

        for command in [
            format!("X='{pattern}'; rm -rf \"$X\""),
            format!("X='{pattern}'; rm -rf $X-suffix"),
        ] {
            assert!(evaluate_command(&command).is_none(), "{command}");
        }
    }

    #[test]
    fn dynamic_flag_classification_resolves_the_complete_word() {
        for command in [
            "X=; rm $X-rf /",
            "rm ${X:-}-rf /",
            "rm ${X:-safe}-rf /",
            "X=; rm ${X-safe}-rf /",
            "X=value; rm -f ${X:+-rf} /",
            "X=; rm -f ${X+-rf} /",
            "X=; printf %s \"${X:=-rf}\"; rm -f \"$X\" /",
            "X=; if true; then true; fi >\"${X:=-rf}\"; rm -f \"$X\" /",
            "rm -rf -- ${X:-}-rf",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        for command in [
            "X=safe; rm $X-rf /",
            "X=; rm safe$X-rf /",
            "X=; rm ${X:-safe}-rf /",
            "X=; rm -f ${X:+-rf} /",
            "X=value; rm ${X+safe}-rf /",
            "X=; rm -f -- $X-rf /",
            "X=; rm -rf -- $X-rf",
            "X=; rm -rf -- ${X:-}-rf",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn fallback_resolution_uses_the_tracked_value_before_the_default() {
        for (command, expected_rule) in [
            (
                "X=-rf; rm -f ${X:-safe} /",
                "unsafe-recursive-delete-expansion",
            ),
            (
                "X=/; rm -rf \"${X:-/tmp}\"",
                "unsafe-recursive-delete-expansion",
            ),
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, expected_rule, "{command}");
        }
    }

    #[test]
    fn nested_assign_default_side_effects_invalidate_tracked_state() {
        for command in [
            "X=; Y=a; printf %s \"${Y%${X:=-rf}}\"; rm -f \"$X\" /",
            "X=; Y=a; printf %s \"${Y/${X:=-rf}/b}\"; rm -f \"$X\" /",
            "X=; Y=a; printf %s \"${Y^${X:=-rf}}\"; rm -f \"$X\" /",
            "X=; Y=a; printf %s \"${Y:${X:=1}}\"; rm -f ${X:+-rf} /",
            "X=; Y=a; printf %s \"${Y[${X:=1}]}\"; rm -f ${X:+-rf} /",
            "X=; Y=a; printf %s \"$(( ${X:=1} ))\"; rm -f ${X:+-rf} /",
            "X=; (( ${X:=1} )); rm -f ${X:+-rf} /",
            "X=; for (( i=${X:=1}; i<1; i++ )); do :; done; rm -f ${X:+-rf} /",
            "X=; for Y in \"${X:=1}\"; do :; done; rm -f ${X:+-rf} /",
            "X=; Y=a; printf %s $\"${Y%${X:=-rf}}\"; rm -f \"$X\" /",
            "X=; Y=a; printf x >\"${Y%${X:=-rf}}\"; rm -f \"$X\" /",
            "X=; Y=a; [[ \"${Y%${X:=-rf}}\" == a ]]; rm -f \"$X\" /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        let command = "X=; printf %s $'${X:=-rf}'; rm -f \"$X\" /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn ansi_c_escaped_command_position_denies() {
        for command in [
            "$'\\x72\\x6d' --no-preserve-root -rf /",
            "$'r\\155' --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn irreversible_home_delete_denies() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut commands = vec![
            "rm -rf ~".to_string(),
            "/bin/rm -rf ~/work".to_string(),
            "rm -Rf $HOME".to_string(),
            "rm --recursive --force $HOME".to_string(),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            commands.push(format!("rm -Rf {}/./", Path::new(&home).display()));
        }
        for command in commands {
            let deny = evaluate_command(&command).unwrap();
            assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
        }
    }

    #[test]
    fn unresolved_or_root_expansion_delete_denies() {
        for command in [
            "rm -rf \"$UNSET\"",
            "ROOT=/; rm -rf \"$ROOT\"",
            "ROOT=/; rm -rf \"$ROOT\"/*",
            "EMPTY=; rm -rf \"$EMPTY\"",
            "EMPTY=; rm -rf \"$EMPTY\"/*",
            "rm -rf \"${UNSET:-/}\"",
            "rm -rf \"${UNSET:=/}\"",
            "rm -rf \"${SET:+/}\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn command_substitution_in_rm_arguments_denies() {
        for command in [
            "rm -rf \"$(resolve-target)\"",
            "rm -rf `resolve-target`",
            "rm -rf \"prefix-$(resolve-target)\"",
            "rm -rf \"$((1 + $(resolve-target)))\"",
            "rm $(printf '%s\\n' -rf /)",
            "rm `printf '%s\\n' -rf /`",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn command_substitution_in_any_argument_denies() {
        for command in [
            "printf '%s' \"$(rm --no-preserve-root -rf /)\"",
            "echo $(rm --no-preserve-root -rf /)",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn execution_bearing_shell_syntax_denies() {
        for command in [
            "cat <(rm --no-preserve-root -rf /)",
            "cat >(rm --no-preserve-root -rf /)",
            "{ rm --no-preserve-root -rf /; }",
            "(rm --no-preserve-root -rf /)",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert!(
                matches!(
                    deny.rule_id,
                    "unsafe-recursive-delete-expansion" | "irreversible-root-delete"
                ),
                "{command}: {}",
                deny.rule_id
            );
        }
    }

    #[test]
    fn pathname_expanded_command_position_denies() {
        let command = "/bin/r[m] --no-preserve-root -rf /";
        let deny = evaluate_command(command).unwrap();
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn quote_fragmented_pathname_pattern_cannot_hide_a_command() {
        let command = "/bin/r[\"m\"] --no-preserve-root -rf /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn quote_fragmented_brace_pattern_cannot_hide_a_root_target() {
        let command = "rm --no-preserve-root -rf /{'',}";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn negated_initial_closing_bracket_patterns_cannot_hide_a_command() {
        for command in [
            "/bin/r[!]] --no-preserve-root -rf /",
            "/bin/r[^]] --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn escaped_pathname_class_command_position_denies() {
        let command = "/bin/r[\\m] --no-preserve-root -rf /";
        let deny = evaluate_command(command).unwrap();
        assert_eq!(deny.rule_id, "unsafe-recursive-delete-expansion");
    }

    #[test]
    fn bracket_test_command_is_inert() {
        assert!(evaluate_command("[ 1 = 1 ]").is_none());
    }

    #[test]
    fn bare_arithmetic_command_is_inert() {
        assert!(evaluate_command("((1+1))").is_none());
    }

    #[test]
    fn arithmetic_expansion_command_is_inert() {
        assert!(evaluate_command("$((2*2))").is_none());
    }

    #[test]
    fn arithmetic_ternary_command_is_inert() {
        assert!(evaluate_command("$((1 ? 2 : 3))").is_none());
    }

    #[test]
    fn quoted_or_escaped_execution_syntax_is_inert() {
        for command in [
            "printf '%s' '<(rm --no-preserve-root -rf /)'",
            "printf '%s' '>(rm --no-preserve-root -rf /)'",
            "printf '%s' '/bin/r[m]'",
            "printf '%s' '{ rm -rf /; }'",
            "printf '%s' \\<\\(rm\\ -rf\\ /\\)",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn quoted_command_word_is_not_a_redirection() {
        assert!(evaluate_command("'>/tmp/not-a-redirection' rm -rf /").is_none());
    }

    #[test]
    fn quoted_redirection_lookalike_does_not_hide_the_command_word() {
        assert!(evaluate_command("'>sentinel'>/dev/null rm -rf /").is_none());
    }

    #[test]
    fn unmatched_brace_text_is_inert() {
        assert!(evaluate_command("printf %s foo}").is_none());
    }

    #[test]
    fn variable_expanded_command_position_denies() {
        for command in [
            "CMD=rm; $CMD --no-preserve-root -rf /",
            "COMMAND=/bin/rm; ${COMMAND} --no-preserve-root -rf /",
            "CMD=rm; >/dev/null $CMD --no-preserve-root -rf /",
            "CMD=rm; > /dev/null $CMD --no-preserve-root -rf /",
            "CMD=rm; exec $CMD --no-preserve-root -rf /",
            "CMD=rm; ! $CMD --no-preserve-root -rf /",
            "CMD=rm; if $CMD --no-preserve-root -rf /; then :; fi",
            "CMD=rm; while $CMD --no-preserve-root -rf /; do :; done",
            "CMD=rm; until $CMD --no-preserve-root -rf /; do :; done",
            "CMD=rm; time $CMD --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn variable_expanded_command_after_supported_wrappers_denies() {
        for command in [
            "CMD=rm; sudo FOO=bar $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo >/dev/null -u root $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo -u >/dev/null root $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo -nu root $CMD --no-preserve-root -rf /",
            "CMD=rm; exec >/dev/null -a fake $CMD --no-preserve-root -rf /",
            "CMD=rm; command >/dev/null -- $CMD --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        for command in [
            "CMD=rm; env -iC /tmp $CMD --no-preserve-root -rf /",
            "CMD=rm; env >/dev/null -iC /tmp $CMD --no-preserve-root -rf /",
            "CMD=rm; env -iC >/dev/null /tmp $CMD --no-preserve-root -rf /",
        ] {
            assert!(
                matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
                "{command}"
            );
        }
    }

    #[test]
    fn abbreviated_gnu_rm_recursive_option_denies_root_delete() {
        let command = "rm --rec --no-preserve-root /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete");
    }

    #[test]
    fn clustered_exec_options_reach_the_wrapped_delete() {
        let command = "exec -ca display rm -rf /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete");
    }

    #[test]
    fn abbreviated_gnu_time_value_option_is_indeterminate() {
        assert!(matches!(
            evaluate_result("/usr/bin/time --out log rm -rf /"),
            SafetyEvaluation::Indeterminate(_)
        ));
    }

    #[test]
    fn command_substitution_in_execution_control_positions_denies() {
        for command in [
            "TARGET=$(printf /); rm -rf \"$TARGET\"",
            "TARGET=`printf /`; rm -rf \"$TARGET\"",
            "$(printf rm) -rf /",
            "`printf rm` -rf /",
            "env TARGET=$(printf /) rm -rf \"$TARGET\"",
            "env TARGET=`printf /` rm -rf \"$TARGET\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn literal_env_split_string_execution_denies() {
        for command in [
            "env -S 'rm -rf /'",
            "env -S'rm -rf /'",
            "env -iS 'rm -rf /'",
            "env -iS'rm -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }

        for command in [
            "env --split-string 'rm -rf /'",
            "env --split-string='rm -rf /'",
            "env --split 'rm -rf /'",
            "env --split='rm -rf /'",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax),
                "{command}"
            );
        }
    }

    #[test]
    fn command_substitution_in_env_split_string_denies() {
        for command in [
            "env -S \"$(printf 'rm -rf /')\"",
            "env -S\"$(printf 'rm -rf /')\"",
            "env --split-string \"$(printf 'rm -rf /')\"",
            "env --split-string=\"$(printf 'rm -rf /')\"",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
    }

    #[test]
    fn continued_dollar_command_substitution_is_indeterminate_without_preprocessing() {
        // Brush 0.4.0 tokenizes `$\\\n(` as a word ending in `$` followed by a
        // separate `(` operator. Do not pre-remove line continuations here:
        // Bash preserves them inside single quotes and quoted heredocs.
        for command in [
            "TARGET=$\\\n(printf /); rm -rf \"$TARGET\"",
            "$\\\n(printf rm) -rf /",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax),
                "{command:?}",
            );
        }
    }

    #[test]
    fn double_quoted_continued_command_substitution_still_denies() {
        for command in [
            "TARGET=\"$\\\n(printf /)\"; rm -rf \"$TARGET\"",
            "\"$\\\n(printf rm)\" -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command:?}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command:?}"
            );
        }
    }

    #[test]
    fn arithmetic_expansion_has_no_deterministic_decision() {
        for command in [
            "rm -rf \"$((1+1))\"",
            "rm -rf $((1+1))",
            "TARGET=$((1+1)); echo \"$TARGET\"",
            "$((1+1)) -rf /",
            "TARGET=$(\\\n(1+1)); echo \"$TARGET\"",
            "$(\\\n(1+1)) -rf /",
            "$[1+1] -rf /",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn continued_dollar_arithmetic_expansion_is_indeterminate_without_preprocessing() {
        // This is the arithmetic counterpart of Brush 0.4.0's `$\\\n(` token
        // split. Parser failure preserves provider-native confirmation.
        for command in [
            "TARGET=$\\\n((1+1)); echo \"$TARGET\"",
            "$\\\n((1+1)) -rf /",
        ] {
            assert_eq!(
                evaluate_result(command),
                SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax),
                "{command:?}",
            );
        }
    }

    #[test]
    fn inert_command_substitution_syntax_has_no_deterministic_decision() {
        for command in [
            "rm -rf '$(resolve-target)'",
            "rm -rf '`resolve-target`'",
            "rm -rf \"\\$(resolve-target)\"",
            "rm -rf \\`resolve-target\\`",
            "TARGET='$(printf /)'; echo \"$TARGET\"",
            "'$(printf rm)' -rf /",
            "$'$(rm --no-preserve-root -rf /)' ignored",
            "$'printf' hello",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn ordinary_command_has_no_deterministic_decision() {
        for command in [
            "cargo test",
            "rm -rf target",
            "echo 'rm -rf /'",
            "rm -rf '$HOME'",
            "rm -rf '${HOME}/work'",
            "rm -rf '~'",
            "TMPDIR=; rm -rf \"${TMPDIR:-/tmp}/work\"",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn quoted_or_escaped_root_globs_remain_literal_targets() {
        for command in ["rm -rf '/*'", "rm -rf /\\*"] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn missing_command_capability_has_no_deterministic_decision() {
        assert_eq!(
            evaluate_in_process(None),
            SafetyEvaluation::NoDeterministicDecision
        );
    }
}
