use std::collections::HashMap;
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
#[cfg(unix)]
const MAX_TRUSTED_HOME_BYTES: usize = 4_096;

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
    if home.as_os_str().as_bytes().len() > MAX_TRUSTED_HOME_BYTES || home.to_str().is_none() {
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
                .stdin(Stdio::from(helper_input))
                .env_clear()
                .env("HOME", trusted_home);
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

fn run_helper_with(
    mut reader: impl std::io::Read,
    mut writer: impl std::io::Write,
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
    let evaluation = evaluate_in_process(Some(&ShellCommandInput {
        dialect: ShellDialect::Bash,
        source,
    }));
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
    let Some(input) = command else {
        return SafetyEvaluation::NoDeterministicDecision;
    };
    if input.dialect != ShellDialect::Bash {
        return SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedDialect);
    }

    let program = match shell::analyze(&input.source) {
        Ok(program) => program,
        Err(error) => return SafetyEvaluation::Indeterminate(error),
    };
    if program.features.command_substitution
        || program.features.process_substitution
        || program.features.executable_group
    {
        return dynamic_execution_deny();
    }

    let mut assignments = HashMap::new();
    if let Ok(home) = std::env::var("HOME") {
        assignments.insert("HOME".into(), home);
    }
    let mut ifs_unknown = false;
    for command in &program.commands {
        if matches!(
            command.context,
            shell::ExecutionContext::TopLevel
                | shell::ExecutionContext::Conditional
                | shell::ExecutionContext::Loop
        ) {
            for name in &command.invalidated_assignments {
                assignments.remove(name);
                if name == "IFS" {
                    ifs_unknown = true;
                }
            }
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
                                if let Some(current) = assignments.get_mut(&assignment.name) {
                                    current.push_str(value);
                                    true
                                } else {
                                    false
                                }
                            }
                            Some(value) => {
                                assignments.insert(assignment.name.clone(), value.to_string());
                                true
                            }
                            None => false,
                        };
                        if value_known {
                            if assignment.name == "IFS" {
                                ifs_unknown = false;
                            }
                        } else {
                            assignments.remove(&assignment.name);
                            if assignment.name == "IFS" {
                                ifs_unknown = true;
                            }
                        }
                    }
                    shell::ExecutionContext::Conditional | shell::ExecutionContext::Loop => {
                        assignments.remove(&assignment.name);
                        if assignment.name == "IFS" {
                            ifs_unknown = true;
                        }
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
                || command.context == shell::ExecutionContext::Pipeline
            {
                assignments.clear();
                ifs_unknown = true;
            }
            continue;
        };

        let command_assignments = assignments.clone();
        let command_ifs_unknown = ifs_unknown;
        if matches!(
            command.context,
            shell::ExecutionContext::TopLevel
                | shell::ExecutionContext::Conditional
                | shell::ExecutionContext::Loop
                | shell::ExecutionContext::Pipeline
        ) {
            assignments.clear();
            ifs_unknown = true;
        }

        let mut words = Vec::with_capacity(command.arguments.len() + 1);
        words.push(command_word);
        words.extend(&command.arguments);
        let words = match unwrap_command(&words) {
            Ok(words) => words,
            Err(()) => return dynamic_execution_deny(),
        };
        let Some(command_word) = words.first() else {
            continue;
        };
        let Some(command_name_literal) = command_word.literal.as_deref() else {
            if word_can_select_command(command_word) {
                return dynamic_execution_deny();
            }
            continue;
        };
        if command_name(command_name_literal) != "rm" {
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
                    ) =>
                {
                    return dynamic_execution_deny();
                }
                None => {}
            }
        }
        if !recursive {
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
                if literal_home_target(target) {
                    return canonical_deny("irreversible-home-delete");
                }
                continue;
            }
            if word_is_home_target(target, &command_assignments) {
                return canonical_deny("irreversible-home-delete");
            }
            if resolve_word(target, &command_assignments)
                .is_some_and(|resolved| literal_home_target(&resolved))
            {
                return canonical_deny("irreversible-home-delete");
            }
            if dynamic_target_is_dangerous(target, &command_assignments, command_ifs_unknown) {
                return expansion_target_deny();
            }
        }
    }
    SafetyEvaluation::NoDeterministicDecision
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

fn literal_home_target(target: &str) -> bool {
    let Some(target) = lexical_absolute_parts(Path::new(target)) else {
        return false;
    };
    std::env::var_os("HOME")
        .and_then(|home| lexical_absolute_parts(Path::new(&home)))
        .is_some_and(|home| target == home)
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
) -> bool {
    if target.can_split_fields {
        return resolve_word_fields(target, assignments, ifs_unknown).is_none_or(|fields| {
            fields.iter().any(|field| {
                is_root_target(&field.value)
                    || literal_home_target(&field.value)
                    || parameter_pattern_may_match_home(field)
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

fn parameter_pattern_may_match_home(field: &ResolvedField) -> bool {
    field.parameter_pathname_pattern
        && std::env::var("HOME")
            .ok()
            .is_some_and(|home| pattern_may_match_literal(&field.value, &home))
}

fn parameter_pattern_may_supply_flag(field: &ResolvedField) -> bool {
    if !field.parameter_pathname_pattern {
        return false;
    }
    let (prefix, _) = conservative_pattern_envelope(&field.value);
    prefix.is_empty() || prefix.starts_with('-')
}

fn pattern_may_match_literal(pattern: &str, literal: &str) -> bool {
    let (prefix, suffix) = conservative_pattern_envelope(pattern);
    let suffix_may_match =
        suffix.is_empty() || suffix.starts_with('/') || literal.ends_with(suffix);
    suffix_may_match
        && (literal.starts_with(prefix)
            || (Path::new(pattern).is_absolute()
                && pattern
                    .split('/')
                    .any(|component| matches!(component, "." | ".."))))
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

fn word_is_home_target(target: &shell::ShellWord, assignments: &HashMap<String, String>) -> bool {
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
        Some(home) => std::env::var_os("HOME")
            .and_then(|trusted| lexical_absolute_parts(Path::new(&trusted)))
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
) -> bool {
    resolve_word(word, assignments).map_or_else(
        || word_is_home_target(word, assignments),
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

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn command_name(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

fn unwrap_command<'a>(
    mut words: &'a [&'a shell::ShellWord],
) -> Result<&'a [&'a shell::ShellWord], ()> {
    loop {
        while words
            .first()
            .is_some_and(|word| word.literal.as_deref().is_some_and(is_assignment))
        {
            words = &words[1..];
        }
        let Some(wrapper) = words.first() else {
            return Ok(words);
        };
        let Some(wrapper) = wrapper.literal.as_deref() else {
            return if word_can_select_command(wrapper) {
                Err(())
            } else {
                Ok(words)
            };
        };
        match command_name(wrapper) {
            "time" => {
                words = &words[1..];
                while let Some(word) = words.first() {
                    match word.literal.as_deref() {
                        Some("--") => {
                            words = &words[1..];
                            break;
                        }
                        Some(option) if option.starts_with('-') && option != "-" => {
                            let takes_value = time_option_takes_separate_value(option);
                            words = &words[1..];
                            if takes_value && !words.is_empty() {
                                words = &words[1..];
                            }
                        }
                        None if word_can_select_command(word) => return Err(()),
                        _ => break,
                    }
                }
            }
            "exec" => {
                words = &words[1..];
                while let Some(option) = words.first() {
                    let Some(option) = option.literal.as_deref() else {
                        return if word_can_select_command(option) {
                            Err(())
                        } else {
                            Ok(words)
                        };
                    };
                    if option == "--" {
                        words = &words[1..];
                        break;
                    }
                    if is_assignment(option) {
                        words = &words[1..];
                        continue;
                    }
                    if !option.starts_with('-') || option == "-" {
                        break;
                    }
                    let takes_value = exec_option_takes_separate_value(option);
                    words = &words[1..];
                    if takes_value && !words.is_empty() {
                        words = &words[1..];
                    }
                }
            }
            "sudo" => {
                words = &words[1..];
                while let Some(option) = words.first() {
                    let Some(option) = option.literal.as_deref() else {
                        return if word_can_select_command(option) {
                            Err(())
                        } else {
                            Ok(words)
                        };
                    };
                    if option == "--" {
                        words = &words[1..];
                        break;
                    }
                    if is_assignment(option) {
                        words = &words[1..];
                        continue;
                    }
                    if !option.starts_with('-') || option == "-" {
                        break;
                    }
                    let takes_value = sudo_option_takes_separate_value(option);
                    words = &words[1..];
                    if takes_value && !words.is_empty() {
                        words = &words[1..];
                    }
                }
            }
            "command" => {
                words = &words[1..];
                while let Some(word) = words.first() {
                    match word.literal.as_deref() {
                        Some(option) if option.starts_with('-') && option != "-" => {
                            words = &words[1..];
                        }
                        None if word_can_select_command(word) => return Err(()),
                        _ => break,
                    }
                }
            }
            "env" => {
                words = &words[1..];
                while let Some(word) = words.first() {
                    let Some(literal) = word.literal.as_deref() else {
                        return if word_can_select_command(word) {
                            Err(())
                        } else {
                            Ok(words)
                        };
                    };
                    if literal == "--" {
                        words = &words[1..];
                        break;
                    } else if literal == "-" || is_assignment(literal) {
                        words = &words[1..];
                    } else if literal.starts_with('-') && literal != "-" {
                        match classify_env_option(literal) {
                            EnvOption::Flag => words = &words[1..],
                            EnvOption::TakesSeparateValue => {
                                words = &words[1..];
                                if !words.is_empty() {
                                    words = &words[1..];
                                }
                            }
                            EnvOption::SplitString => return Err(()),
                        }
                    } else {
                        break;
                    }
                }
            }
            _ => return Ok(words),
        }
    }
}

fn time_option_takes_separate_value(word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        return !attached
            && !name.is_empty()
            && ["output", "format"]
                .into_iter()
                .any(|option| option.starts_with(name));
    }
    let mut options = word
        .strip_prefix('-')
        .unwrap_or_default()
        .chars()
        .peekable();
    while let Some(option) = options.next() {
        if matches!(option, 'o' | 'f') {
            return options.peek().is_none();
        }
    }
    false
}

fn exec_option_takes_separate_value(word: &str) -> bool {
    let mut options = word
        .strip_prefix('-')
        .unwrap_or_default()
        .chars()
        .peekable();
    while let Some(option) = options.next() {
        if option == 'a' {
            return options.peek().is_none();
        }
    }
    false
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _)| is_variable_name(name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvOption {
    Flag,
    TakesSeparateValue,
    SplitString,
}

fn classify_env_option(word: &str) -> EnvOption {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        if !name.is_empty() && "split-string".starts_with(name) {
            return EnvOption::SplitString;
        }
        if !attached && !name.is_empty() && ("unset".starts_with(name) || "chdir".starts_with(name))
        {
            return EnvOption::TakesSeparateValue;
        }
        return EnvOption::Flag;
    }

    let mut options = word
        .strip_prefix('-')
        .unwrap_or_default()
        .chars()
        .peekable();
    while let Some(option) = options.next() {
        match option {
            'S' => return EnvOption::SplitString,
            'u' | 'C' if options.peek().is_none() => return EnvOption::TakesSeparateValue,
            'u' | 'C' => return EnvOption::Flag,
            _ => {}
        }
    }
    EnvOption::Flag
}

fn sudo_option_takes_separate_value(word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        return !attached
            && !name.is_empty()
            && [
                "user",
                "group",
                "host",
                "prompt",
                "close-from",
                "command-timeout",
                "chroot",
                "chdir",
                "role",
                "type",
                "other-user",
            ]
            .into_iter()
            .any(|option| option.starts_with(name));
    }

    let mut options = word
        .strip_prefix('-')
        .unwrap_or_default()
        .chars()
        .peekable();
    while let Some(option) = options.next() {
        if matches!(option, 'u' | 'g' | 'h' | 'p' | 'C' | 'T' | 'R' | 'D' | 't') {
            return options.peek().is_none();
        }
    }
    false
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
                "env $'-\\x53' 'rm -rf /'",
                "unsafe-recursive-delete-expansion",
            ),
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
            "env -iC/tmp rm -rf /",
            "env --chdir=/tmp rm -rf /",
            "env FOO=bar -- rm -rf /",
            "env 'FOO=bar' rm -rf /",
            "env - rm -rf /",
            "exec -a displayed rm -rf /",
            "exec -a NAME=foo rm -rf /",
            "command -- rm -rf /",
            "time -p rm -rf /",
            "/usr/bin/time -o log rm -rf /",
            "/usr/bin/time -vo log rm -rf /",
            "/usr/bin/time -vf FORMAT rm -rf /",
            "/usr/bin/time -volog rm -rf /",
            "/usr/bin/time -vfFORMAT rm -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
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
    fn arithmetic_and_pipeline_parent_mutation_invalidate_tracked_assignments() {
        for command in [
            "X=; ((X=1)); rm -f \"${X:+-rf}\" /",
            "shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
            );
        }
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
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
                "{command}"
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
            "CMD=rm; env -iC /tmp $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo >/dev/null -u root $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo -u >/dev/null root $CMD --no-preserve-root -rf /",
            "CMD=rm; sudo -nu root $CMD --no-preserve-root -rf /",
            "CMD=rm; env >/dev/null -iC /tmp $CMD --no-preserve-root -rf /",
            "CMD=rm; env -iC >/dev/null /tmp $CMD --no-preserve-root -rf /",
            "CMD=rm; exec >/dev/null -a fake $CMD --no-preserve-root -rf /",
            "CMD=rm; command >/dev/null -- $CMD --no-preserve-root -rf /",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
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
    fn abbreviated_gnu_time_value_option_reaches_the_wrapped_delete() {
        let command = "/usr/bin/time --out log rm -rf /";
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete");
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
            "env --split-string 'rm -rf /'",
            "env --split-string='rm -rf /'",
            "env --split 'rm -rf /'",
            "env --split='rm -rf /'",
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(
                deny.rule_id, "unsafe-recursive-delete-expansion",
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
