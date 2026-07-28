use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafetyDeny {
    pub rule_id: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellWord {
    text: String,
    variable_expansion: bool,
    tilde_expansion: bool,
    command_substitution: bool,
}

pub(crate) fn evaluate(command: Option<&str>) -> Option<SafetyDeny> {
    let input = command?;

    let mut assignments = HashMap::new();
    for command in tokenize_commands(input) {
        let mut words = command.as_slice();
        let mut dynamic_execution = false;
        while let Some((name, value)) = words.first().and_then(|word| word.text.split_once('=')) {
            if is_variable_name(name) {
                dynamic_execution |= words[0].command_substitution;
                assignments.insert(name.to_string(), value.to_string());
                words = &words[1..];
            } else {
                break;
            }
        }
        if words.is_empty() {
            if dynamic_execution {
                return Some(SafetyDeny {
                    rule_id: "unsafe-recursive-delete-expansion",
                    reason: "refusing execution through an unresolved command substitution".into(),
                });
            }
            continue;
        }
        words = unwrap_command(words, &mut dynamic_execution);
        dynamic_execution |= words.first().is_some_and(|word| word.command_substitution);
        if dynamic_execution {
            return Some(SafetyDeny {
                rule_id: "unsafe-recursive-delete-expansion",
                reason: "refusing execution through an unresolved command substitution".into(),
            });
        }
        if words.first().map(|word| command_name(&word.text)) != Some("rm") {
            continue;
        }
        let args = &words[1..];
        if args.iter().any(|argument| argument.command_substitution) {
            return Some(SafetyDeny {
                rule_id: "unsafe-recursive-delete-expansion",
                reason: "refusing deletion through an unresolved command substitution".into(),
            });
        }
        if !args.iter().any(|arg| is_recursive_flag(&arg.text)) {
            continue;
        }
        for target in args.iter().filter(|arg| !arg.text.starts_with('-')) {
            if is_root_target(&target.text) {
                return Some(SafetyDeny {
                    rule_id: "irreversible-root-delete",
                    reason: "refusing recursive deletion of the filesystem root".into(),
                });
            }
            if is_home_target(target) {
                return Some(SafetyDeny {
                    rule_id: "irreversible-home-delete",
                    reason: "refusing recursive deletion of the home directory".into(),
                });
            }
            if parameter_default_is_dangerous(target) {
                return Some(SafetyDeny {
                    rule_id: "unsafe-recursive-delete-expansion",
                    reason: "refusing recursive deletion through an unresolved, empty, or root-valued expansion".into(),
                });
            }
            if expansion_is_unresolved_empty_or_root(target, &assignments) {
                return Some(SafetyDeny {
                    rule_id: "unsafe-recursive-delete-expansion",
                    reason: "refusing recursive deletion through an unresolved, empty, or root-valued expansion".into(),
                });
            }
        }
    }
    None
}

fn parameter_default_is_dangerous(target: &ShellWord) -> bool {
    if !target.variable_expansion {
        return false;
    }
    let Some(rest) = target.text.strip_prefix("${") else {
        return false;
    };
    let Some(close) = rest.find('}') else {
        return false;
    };
    let expression = &rest[..close];
    let name_end = expression
        .find(|character: char| character != '_' && !character.is_ascii_alphanumeric())
        .unwrap_or(expression.len());
    if !is_variable_name(&expression[..name_end]) {
        return false;
    }
    let operator = &expression[name_end..];
    let fallback = [":-", ":=", ":+", "-", "=", "+"]
        .into_iter()
        .find_map(|prefix| operator.strip_prefix(prefix));
    let Some(fallback) = fallback else {
        return false;
    };
    fallback.is_empty() || is_root_target(fallback) || matches!(fallback, "~" | "$HOME" | "${HOME}")
}

fn is_recursive_flag(argument: &str) -> bool {
    argument == "--recursive"
        || (argument.starts_with('-')
            && !argument.starts_with("--")
            && argument[1..].contains(['r', 'R']))
}

fn is_root_target(target: &str) -> bool {
    lexical_absolute_parts(Path::new(strip_root_glob(target))).is_some_and(|parts| parts.is_empty())
}

fn is_home_target(target: &ShellWord) -> bool {
    (target.tilde_expansion && (target.text == "~" || target.text.starts_with("~/")))
        || (target.variable_expansion
            && (target.text == "$HOME"
                || target.text.starts_with("$HOME/")
                || target.text == "${HOME}"
                || target.text.starts_with("${HOME}/")))
        || literal_home_target(&target.text)
}

fn literal_home_target(target: &str) -> bool {
    let Some(target) = lexical_absolute_parts(Path::new(strip_root_glob(target))) else {
        return false;
    };
    std::env::var_os("HOME")
        .and_then(|home| lexical_absolute_parts(Path::new(&home)))
        .is_some_and(|home| target == home)
}

fn strip_root_glob(target: &str) -> &str {
    if target == "/*" {
        "/"
    } else {
        target.strip_suffix("/*").unwrap_or(target)
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

fn expansion_is_unresolved_empty_or_root(
    target: &ShellWord,
    assignments: &HashMap<String, String>,
) -> bool {
    if !target.variable_expansion {
        return false;
    }
    let Some((name, suffix)) = variable_reference(&target.text) else {
        return false;
    };
    assignments.get(name).is_none_or(|value| {
        is_root_target(value) || (value.is_empty() && (suffix.is_empty() || is_root_target(suffix)))
    })
}

fn variable_reference(target: &str) -> Option<(&str, &str)> {
    if let Some(rest) = target.strip_prefix("${") {
        let close = rest.find('}')?;
        let (name, suffix) = rest.split_at(close);
        return is_variable_name(name).then_some((name, &suffix[1..]));
    }
    let rest = target.strip_prefix('$')?;
    let end = rest
        .find(|character: char| character != '_' && !character.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    let (name, suffix) = rest.split_at(end);
    is_variable_name(name).then_some((name, suffix))
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

fn unwrap_command<'a>(mut words: &'a [ShellWord], dynamic_execution: &mut bool) -> &'a [ShellWord] {
    loop {
        match words.first().map(|word| command_name(&word.text)) {
            Some("sudo") => {
                words = &words[1..];
                while let Some(option) = words.first() {
                    if option.text == "--" {
                        words = &words[1..];
                        break;
                    }
                    if !option.text.starts_with('-') || option.text == "-" {
                        break;
                    }
                    let takes_value = matches!(
                        option.text.as_str(),
                        "-u" | "-g"
                            | "-h"
                            | "-p"
                            | "-C"
                            | "-T"
                            | "-R"
                            | "-D"
                            | "-t"
                            | "--user"
                            | "--group"
                            | "--host"
                            | "--prompt"
                            | "--close-from"
                            | "--command-timeout"
                            | "--chroot"
                            | "--chdir"
                            | "--role"
                            | "--type"
                            | "--other-user"
                    );
                    words = &words[1..];
                    if takes_value && !words.is_empty() {
                        words = &words[1..];
                    }
                }
            }
            Some("command") => {
                words = &words[1..];
                while words
                    .first()
                    .is_some_and(|word| word.text.starts_with('-') && word.text != "-")
                {
                    words = &words[1..];
                }
            }
            Some("env") => {
                words = &words[1..];
                while let Some(word) = words.first() {
                    if word.text == "--" {
                        words = &words[1..];
                        break;
                    } else if word.text.starts_with("--split-string=")
                        || (word.text.starts_with("-S") && word.text != "-S")
                    {
                        *dynamic_execution |= word.command_substitution;
                        words = &words[1..];
                    } else if matches!(
                        word.text.as_str(),
                        "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
                    ) {
                        let split_string = matches!(word.text.as_str(), "-S" | "--split-string");
                        words = &words[1..];
                        if !words.is_empty() {
                            if split_string {
                                *dynamic_execution |= words[0].command_substitution;
                            }
                            words = &words[1..];
                        }
                    } else if is_assignment(&word.text) {
                        *dynamic_execution |= word.command_substitution;
                        words = &words[1..];
                    } else if word.text.starts_with('-') {
                        words = &words[1..];
                    } else {
                        break;
                    }
                }
            }
            _ => return words,
        }
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _)| is_variable_name(name))
}

fn tokenize_commands(input: &str) -> Vec<Vec<ShellWord>> {
    let mut commands = Vec::new();
    let mut command = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut variable_expansion = false;
    let mut tilde_expansion = false;
    let mut command_substitution = false;
    let mut quote = None;
    let mut escaped = false;
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        if escaped {
            word.push(character);
            word_started = true;
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            if chars.peek() == Some(&'\n') {
                chars.next();
                continue;
            }
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                if active_quote == '"' {
                    if character == '$' {
                        variable_expansion = true;
                        if command_substitution_follows(chars.clone()) {
                            command_substitution = true;
                        }
                    } else if character == '`' {
                        command_substitution = true;
                    }
                }
                word.push(character);
            }
            word_started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                word_started = true;
            }
            ' ' | '\t' | '\r' => push_word(
                &mut command,
                &mut word,
                &mut word_started,
                &mut variable_expansion,
                &mut tilde_expansion,
                &mut command_substitution,
            ),
            ';' | '\n' => push_command(
                &mut commands,
                &mut command,
                &mut word,
                &mut word_started,
                &mut variable_expansion,
                &mut tilde_expansion,
                &mut command_substitution,
            ),
            '&' | '|' => {
                if chars.peek() == Some(&character) {
                    chars.next();
                }
                push_command(
                    &mut commands,
                    &mut command,
                    &mut word,
                    &mut word_started,
                    &mut variable_expansion,
                    &mut tilde_expansion,
                    &mut command_substitution,
                );
            }
            '$' => {
                variable_expansion = true;
                command_substitution |= command_substitution_follows(chars.clone());
                word_started = true;
                word.push(character);
            }
            '`' => {
                command_substitution = true;
                word_started = true;
                word.push(character);
            }
            '~' if !word_started => {
                tilde_expansion = true;
                word_started = true;
                word.push(character);
            }
            _ => {
                word_started = true;
                word.push(character);
            }
        }
    }
    if escaped {
        word.push('\\');
    }
    push_command(
        &mut commands,
        &mut command,
        &mut word,
        &mut word_started,
        &mut variable_expansion,
        &mut tilde_expansion,
        &mut command_substitution,
    );
    commands
}

fn command_substitution_follows<I>(mut chars: std::iter::Peekable<I>) -> bool
where
    I: Iterator<Item = char>,
{
    next_shell_character(&mut chars) == Some('(') && next_shell_character(&mut chars) != Some('(')
}

fn next_shell_character<I>(chars: &mut std::iter::Peekable<I>) -> Option<char>
where
    I: Iterator<Item = char>,
{
    loop {
        match chars.next() {
            Some('\\') if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            character => return character,
        }
    }
}

fn push_word(
    command: &mut Vec<ShellWord>,
    word: &mut String,
    word_started: &mut bool,
    variable_expansion: &mut bool,
    tilde_expansion: &mut bool,
    command_substitution: &mut bool,
) {
    if *word_started {
        command.push(ShellWord {
            text: std::mem::take(word),
            variable_expansion: std::mem::take(variable_expansion),
            tilde_expansion: std::mem::take(tilde_expansion),
            command_substitution: std::mem::take(command_substitution),
        });
        *word_started = false;
    }
}

fn push_command(
    commands: &mut Vec<Vec<ShellWord>>,
    command: &mut Vec<ShellWord>,
    word: &mut String,
    word_started: &mut bool,
    variable_expansion: &mut bool,
    tilde_expansion: &mut bool,
    command_substitution: &mut bool,
) {
    push_word(
        command,
        word,
        word_started,
        variable_expansion,
        tilde_expansion,
        command_substitution,
    );
    if !command.is_empty() {
        commands.push(std::mem::take(command));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate_command(command: &str) -> Option<SafetyDeny> {
        evaluate(Some(command))
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
        ] {
            let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
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
    fn continued_command_substitution_in_execution_control_positions_denies() {
        for command in [
            "TARGET=$\\\n(printf /); rm -rf \"$TARGET\"",
            "TARGET=\"$\\\n(printf /)\"; rm -rf \"$TARGET\"",
            "$\\\n(printf rm) -rf /",
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
            "TARGET=$\\\n((1+1)); echo \"$TARGET\"",
            "$\\\n((1+1)) -rf /",
            "TARGET=$(\\\n(1+1)); echo \"$TARGET\"",
            "$(\\\n(1+1)) -rf /",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
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
            "rm -rf \"${TMPDIR:-/tmp}/work\"",
        ] {
            assert!(evaluate_command(command).is_none(), "{command}");
        }
    }

    #[test]
    fn missing_command_capability_has_no_deterministic_decision() {
        assert!(evaluate(None).is_none());
    }
}
