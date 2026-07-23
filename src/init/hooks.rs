use std::collections::BTreeMap;
use std::io;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};

/// One managed Codex hook definition.
struct HookSpec {
    event: &'static str,
    matcher: Option<&'static str>,
    argument: &'static str,
    timeout: u32,
    status_message: Option<&'static str>,
}

const HOOKS: &[HookSpec] = &[
    HookSpec {
        event: "SessionStart",
        matcher: Some("startup|resume|clear|compact"),
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "UserPromptSubmit",
        matcher: None,
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "PreToolUse",
        matcher: Some("*"),
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "PermissionRequest",
        matcher: Some("*"),
        argument: "--permission-hook",
        timeout: 30,
        status_message: Some("Brain reviewing permission…"),
    },
    HookSpec {
        event: "PostToolUse",
        matcher: Some("*"),
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "SubagentStart",
        matcher: Some("*"),
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "SubagentStop",
        matcher: Some("*"),
        argument: "--lifecycle-hook",
        timeout: 2,
        status_message: None,
    },
    HookSpec {
        event: "Stop",
        matcher: None,
        argument: "--recovery-hook",
        timeout: 30,
        status_message: None,
    },
];

const PERMISSION_STATUS_MESSAGE: &str = "Brain reviewing permission…";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagedHookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    SubagentStart,
    SubagentStop,
    Stop,
}

impl ManagedHookEvent {
    pub const ALL: [Self; 8] = [
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PermissionRequest,
        Self::PostToolUse,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::Stop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedHookEventState {
    pub configured: bool,
    pub current: bool,
    pub stale: bool,
    pub disabled: bool,
    pub unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleHookScope {
    pub events: BTreeMap<ManagedHookEvent, ManagedHookEventState>,
}

impl Default for LifecycleHookScope {
    fn default() -> Self {
        Self {
            events: ManagedHookEvent::ALL
                .into_iter()
                .map(|event| (event, ManagedHookEventState::default()))
                .collect(),
        }
    }
}

impl LifecycleHookScope {
    #[cfg(test)]
    pub fn configured(&self) -> bool {
        self.events.values().any(|state| state.configured)
    }

    #[cfg(test)]
    pub fn definitions_current(&self) -> bool {
        self.events
            .values()
            .all(|state| state.current && !state.stale && !state.disabled && !state.unavailable)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleHookDiscovery {
    pub global: LifecycleHookScope,
    pub project: LifecycleHookScope,
    pub trust_unverified: bool,
}

impl LifecycleHookDiscovery {
    #[cfg(test)]
    pub fn configured(&self) -> bool {
        self.global.configured() || self.project.configured()
    }

    #[cfg(test)]
    pub fn duplicate_scopes(&self) -> bool {
        self.global.configured() && self.project.configured()
    }
}

/// Managed PermissionRequest hook state in one Codex configuration scope.
///
/// A stale or disabled managed entry is still configured: callers use that
/// conservative signal to avoid enabling terminal-input fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionHookScope {
    pub configured: bool,
    pub current: bool,
    pub stale: bool,
    pub disabled: bool,
}

/// Managed PermissionRequest hook state across the active user and project
/// configuration layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionHookDiscovery {
    pub global: PermissionHookScope,
    pub project: PermissionHookScope,
}

impl PermissionHookDiscovery {
    pub fn configured(&self) -> bool {
        self.global.configured || self.project.configured
    }

    pub fn duplicate_scopes(&self) -> bool {
        self.global.configured && self.project.configured
    }
}

fn settings_path(project: bool) -> PathBuf {
    if project {
        PathBuf::from(".codex/hooks.json")
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".codex/hooks.json")
    }
}

pub(crate) fn discover_lifecycle_hooks_at(
    home: Option<&Path>,
    cwd: &Path,
) -> LifecycleHookDiscovery {
    let global = home
        .map(|home| lifecycle_scope_from_paths([home.join(".codex/hooks.json")]))
        .unwrap_or_default();
    let markers = project_root_markers(home);
    let project = lifecycle_scope_from_paths(applicable_project_hook_paths(cwd, &markers));
    let trust_unverified = [&global, &project].into_iter().any(|scope| {
        scope
            .events
            .values()
            .any(|state| state.configured && !state.disabled)
    });
    LifecycleHookDiscovery {
        global,
        project,
        trust_unverified,
    }
}

pub(crate) fn discover_permission_hooks_at(
    home: Option<&Path>,
    cwd: &Path,
) -> PermissionHookDiscovery {
    let global = home
        .map(|home| scope_from_paths([home.join(".codex/hooks.json")]))
        .unwrap_or_default();
    let markers = project_root_markers(home);
    let project = scope_from_paths(applicable_project_hook_paths(cwd, &markers));
    PermissionHookDiscovery { global, project }
}

fn project_root_markers(home: Option<&Path>) -> Vec<String> {
    const DEFAULT: &[&str] = &[".git"];
    let Some(home) = home else {
        return DEFAULT.iter().map(|marker| (*marker).to_owned()).collect();
    };
    let Ok(raw) = std::fs::read_to_string(home.join(".codex/config.toml")) else {
        return DEFAULT.iter().map(|marker| (*marker).to_owned()).collect();
    };
    parse_project_root_markers(&raw)
        .unwrap_or_else(|| DEFAULT.iter().map(|marker| (*marker).to_owned()).collect())
}

fn parse_project_root_markers(raw: &str) -> Option<Vec<String>> {
    let mut section = "";
    let lines = raw.lines().collect::<Vec<_>>();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim();
            index += 1;
            continue;
        }
        let Some((key, first_value)) = line.split_once('=') else {
            index += 1;
            continue;
        };
        if section.is_empty() && key.trim() == "project_root_markers" {
            let mut value = first_value.trim().to_owned();
            while !value.contains(']') && index + 1 < lines.len() {
                index += 1;
                value.push(' ');
                value.push_str(lines[index].split('#').next().unwrap_or_default().trim());
            }
            return parse_marker_array(&value);
        }
        index += 1;
    }
    None
}

fn parse_marker_array(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim();
            item.strip_prefix('"')
                .and_then(|item| item.strip_suffix('"'))
                .or_else(|| {
                    item.strip_prefix('\'')
                        .and_then(|item| item.strip_suffix('\''))
                })
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn applicable_project_hook_paths(cwd: &Path, markers: &[String]) -> Vec<PathBuf> {
    applicable_project_dirs_with_markers(cwd, markers)
        .into_iter()
        .map(|path| path.join(".codex/hooks.json"))
        .collect()
}

pub(crate) fn applicable_project_dirs(home: Option<&Path>, cwd: &Path) -> Vec<PathBuf> {
    applicable_project_dirs_with_markers(cwd, &project_root_markers(home))
}

fn applicable_project_dirs_with_markers(cwd: &Path, markers: &[String]) -> Vec<PathBuf> {
    let root = if markers.is_empty() {
        cwd
    } else {
        cwd.ancestors()
            .find(|path| markers.iter().any(|marker| path.join(marker).exists()))
            .unwrap_or(cwd)
    };
    let mut dirs = cwd
        .ancestors()
        .take_while(|path| *path != root)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    dirs.push(root.to_path_buf());
    dirs.reverse();
    dirs
}

fn scope_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> PermissionHookScope {
    let mut scope = PermissionHookScope::default();
    for path in paths {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        inspect_permission_handlers(&value, &mut scope);
    }
    scope
}

fn lifecycle_scope_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> LifecycleHookScope {
    let mut scope = LifecycleHookScope::default();
    for path in paths {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        inspect_lifecycle_handlers(&value, &mut scope);
    }
    scope
}

fn inspect_lifecycle_handlers(value: &serde_json::Value, scope: &mut LifecycleHookScope) {
    let Some(hooks) = value.get("hooks").and_then(serde_json::Value::as_object) else {
        return;
    };
    for event in ManagedHookEvent::ALL {
        let Some(matchers) = hooks
            .get(event.as_str())
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let spec = HOOKS
            .iter()
            .find(|spec| spec.event == event.as_str())
            .expect("managed hook event must have a specification");
        let state = scope
            .events
            .get_mut(&event)
            .expect("all events initialized");
        for matcher_entry in matchers {
            let matcher_disabled = entry_is_disabled(matcher_entry);
            let matcher = matcher_entry
                .get("matcher")
                .and_then(serde_json::Value::as_str);
            let Some(handlers) = matcher_entry
                .get("hooks")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for handler in handlers {
                let Some(command) = handler.get("command").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if !is_discoverable_managed_command(event, command) {
                    continue;
                }
                state.configured = true;
                state.disabled |= matcher_disabled || entry_is_disabled(handler);
                state.unavailable |= command_uses_missing_absolute_binary(command);
                let current = matcher == spec.matcher
                    && handler.get("type").and_then(serde_json::Value::as_str) == Some("command")
                    && is_exact_current_codex_hook_command(command, spec.argument)
                    && handler.get("timeout").and_then(serde_json::Value::as_u64)
                        == Some(u64::from(spec.timeout))
                    && spec.status_message.is_none_or(|expected| {
                        handler
                            .get("statusMessage")
                            .and_then(serde_json::Value::as_str)
                            == Some(expected)
                    });
                state.current |= current;
                state.stale |= !current;
            }
        }
    }
}

fn is_discoverable_managed_command(event: ManagedHookEvent, command: &str) -> bool {
    let expected = match event {
        ManagedHookEvent::PermissionRequest => "--permission-hook",
        ManagedHookEvent::Stop => "--recovery-hook",
        _ => "--lifecycle-hook",
    };
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    if !is_managed_program(program) {
        return false;
    }
    words.any(|argument| argument == expected)
        || (event == ManagedHookEvent::Stop
            && command
                .split_whitespace()
                .any(|argument| argument == "--lifecycle-hook"))
        || (matches!(
            event,
            ManagedHookEvent::PostToolUse | ManagedHookEvent::Stop
        ) && is_managed_snapshot_command(command))
}

fn command_uses_missing_absolute_binary(command: &str) -> bool {
    let Some(program) = command.split_whitespace().next() else {
        return false;
    };
    let path = Path::new(program);
    path.is_absolute() && !path.exists()
}

fn inspect_permission_handlers(value: &serde_json::Value, scope: &mut PermissionHookScope) {
    let Some(matchers) = value
        .get("hooks")
        .and_then(|hooks| hooks.get("PermissionRequest"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for matcher in matchers {
        let matcher_disabled = entry_is_disabled(matcher);
        let matcher_name = matcher
            .get("matcher")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(handlers) = matcher.get("hooks").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for handler in handlers {
            let Some(command) = handler.get("command").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !contains_managed_permission_flag(command) && !is_managed_permission_command(command)
            {
                continue;
            }
            scope.configured = true;
            scope.disabled |= matcher_disabled || entry_is_disabled(handler);
            let current = matcher_name == "*"
                && handler.get("type").and_then(serde_json::Value::as_str) == Some("command")
                && is_current_permission_command(command)
                && handler.get("timeout").and_then(serde_json::Value::as_u64) == Some(30)
                && handler
                    .get("statusMessage")
                    .and_then(serde_json::Value::as_str)
                    == Some(PERMISSION_STATUS_MESSAGE);
            scope.current |= current;
            scope.stale |= !current;
        }
    }
}

fn entry_is_disabled(value: &serde_json::Value) -> bool {
    value.get("enabled").and_then(serde_json::Value::as_bool) == Some(false)
        || value.get("disabled").and_then(serde_json::Value::as_bool) == Some(true)
}

#[cfg(test)]
fn build_hooks_value() -> serde_json::Value {
    let executable = managed_executable();
    build_hooks_value_for(&executable)
}

#[cfg(test)]
fn managed_executable() -> PathBuf {
    PathBuf::from("coding-brain")
}

#[cfg(test)]
fn build_hooks_value_for(executable: &Path) -> serde_json::Value {
    let mut hooks_map = serde_json::Map::new();

    for spec in HOOKS {
        let command = format!("{} {}", executable.display(), spec.argument);
        let mut hook_entry = serde_json::json!({
            "type": "command",
            "command": command,
            "timeout": spec.timeout,
        });
        if let Some(status_message) = spec.status_message {
            hook_entry["statusMessage"] = serde_json::Value::String(status_message.to_owned());
        }

        let mut matcher_entry = serde_json::json!({ "hooks": [hook_entry] });
        if let Some(matcher) = spec.matcher {
            matcher_entry["matcher"] = serde_json::Value::String(matcher.to_owned());
        }

        let array = hooks_map
            .entry(spec.event)
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(arr) = array {
            arr.push(matcher_entry);
        }
    }

    serde_json::Value::Object(hooks_map)
}

/// Check if codexctl hooks are already present in existing settings.
#[cfg(test)]
fn has_codexctl_hooks(existing: &serde_json::Value) -> bool {
    if let Some(hooks) = existing.get("hooks") {
        if let Some(obj) = hooks.as_object() {
            for (event, matchers) in obj {
                if let Some(arr) = matchers.as_array() {
                    for matcher_entry in arr {
                        if let Some(inner_hooks) = matcher_entry.get("hooks") {
                            if let Some(inner_arr) = inner_hooks.as_array() {
                                for hook in inner_arr {
                                    if let Some(cmd) = hook.get("command") {
                                        if let Some(s) = cmd.as_str() {
                                            if is_managed_command(event, s) {
                                                return true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

fn is_current_program(program: &str) -> bool {
    program == "coding-brain"
        || (Path::new(program).is_absolute() && program.ends_with("/coding-brain"))
}

fn is_legacy_program(program: &str) -> bool {
    program == "codexctl" || (Path::new(program).is_absolute() && program.ends_with("/codexctl"))
}

fn is_managed_program(program: &str) -> bool {
    is_current_program(program) || is_legacy_program(program)
}

fn is_exact_command(command: &str, expected_args: &[&str], predicate: fn(&str) -> bool) -> bool {
    let mut words = command.split_whitespace();
    let Some(executable) = words.next() else {
        return false;
    };
    predicate(executable) && words.eq(expected_args.iter().copied())
}

fn is_exact_current_command(command: &str, expected_args: &[&str]) -> bool {
    is_exact_command(command, expected_args, is_current_program)
}

fn is_exact_current_codex_hook_command(command: &str, argument: &str) -> bool {
    is_exact_current_command(command, &[argument])
        || is_exact_current_command(command, &[argument, "--provider", "codex"])
}

fn is_exact_managed_command(command: &str, expected_args: &[&str]) -> bool {
    is_exact_command(command, expected_args, is_managed_program)
}

fn is_current_permission_command(command: &str) -> bool {
    is_exact_current_codex_hook_command(command, "--permission-hook")
}

fn contains_managed_permission_flag(command: &str) -> bool {
    let mut words = command.split_whitespace();
    words.next().is_some_and(is_managed_program)
        && words.any(|argument| argument == "--permission-hook")
}

fn is_managed_snapshot_command(command: &str) -> bool {
    is_exact_managed_command(command, &["--json"])
        || is_exact_managed_command(command, &["--json", "2>/dev/null", "||", "true"])
}

fn is_managed_permission_command(command: &str) -> bool {
    is_exact_managed_command(command, &["--permission-hook"])
}

#[cfg(test)]
fn is_managed_command(event: &str, command: &str) -> bool {
    match event {
        "PermissionRequest" => is_managed_permission_command(command),
        "Stop" => {
            is_exact_managed_command(command, &["--recovery-hook"])
                || is_exact_managed_command(command, &["--lifecycle-hook"])
                || is_managed_snapshot_command(command)
        }
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SubagentStart"
        | "SubagentStop" => {
            is_exact_managed_command(command, &["--lifecycle-hook"])
                || (event == "PostToolUse" && is_managed_snapshot_command(command))
        }
        _ => false,
    }
}

/// Merge codexctl hooks into existing settings, preserving all other keys
/// and any non-codexctl hooks already defined.
#[cfg(test)]
fn merge_hooks(existing: &mut serde_json::Value) {
    remove_codexctl_hooks(existing);
    let new_hooks = build_hooks_value();

    let hooks_obj = existing
        .as_object_mut()
        .expect("settings must be an object")
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    if let (Some(target), Some(source)) = (hooks_obj.as_object_mut(), new_hooks.as_object()) {
        for (event, new_matchers) in source {
            let event_arr = target
                .entry(event)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let (Some(arr), Some(new_arr)) = (event_arr.as_array_mut(), new_matchers.as_array())
            {
                arr.retain_mut(|matcher| filter_managed_hooks(event, matcher).1);
                for new_matcher in new_arr {
                    arr.push(new_matcher.clone());
                }
            }
        }
    }
}

/// Remove codexctl hooks from a matcher entry's inner hooks array.
/// Returns the number removed and whether any hooks remain after filtering.
#[cfg(test)]
fn filter_managed_hooks(event: &str, matcher_entry: &mut serde_json::Value) -> (usize, bool) {
    if let Some(inner_hooks) = matcher_entry.get_mut("hooks") {
        if let Some(arr) = inner_hooks.as_array_mut() {
            let before = arr.len();
            arr.retain(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_none_or(|command| !is_managed_command(event, command))
            });
            return (before - arr.len(), !arr.is_empty());
        }
    }
    (0, true)
}

/// Remove all codexctl hook entries from settings, preserving everything else.
/// Returns the number of hook entries removed.
#[cfg(test)]
fn remove_codexctl_hooks(settings: &mut serde_json::Value) -> usize {
    let mut removed = 0;

    let Some(hooks) = settings.get_mut("hooks") else {
        return 0;
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return 0;
    };

    // For each event, filter out matcher entries that contain codexctl commands
    let mut empty_events = Vec::new();
    for (event, matchers) in hooks_obj.iter_mut() {
        if let Some(arr) = matchers.as_array_mut() {
            arr.retain_mut(|matcher| {
                let (removed_handlers, keep) = filter_managed_hooks(event, matcher);
                removed += removed_handlers;
                keep
            });
            if arr.is_empty() {
                empty_events.push(event.clone());
            }
        }
    }

    // Remove event keys that are now empty
    for event in empty_events {
        hooks_obj.remove(&event);
    }

    // Remove the hooks key entirely if it's now empty
    if hooks_obj.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }

    removed
}

#[cfg(test)]
fn write_hooks_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.coding-brain.tmp");
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Run the uninit command: remove codexctl hooks from hooks.json.
pub fn run_uninit(project: bool) -> io::Result<()> {
    super::recover_pending_hook_transaction()?;
    let scope = if project {
        super::provider_hooks::HookScope::Project
    } else {
        super::provider_hooks::HookScope::Global
    };
    let plans = super::provider_hooks::stage_provider_hook_removal(
        &[coding_brain_core::provider::AgentProvider::Codex],
        scope,
    )?;
    report_preserved_entries(&plans);
    super::provider_hooks::apply_hook_transaction(&plans)?;
    println!(
        "Removed exact managed Coding Brain hooks from {}",
        settings_path(project).display()
    );
    Ok(())
}

/// Run the init command: write Codex hooks into hooks.json.
pub fn run_init(project: bool, dry_run: bool) -> io::Result<()> {
    super::recover_pending_hook_transaction()?;
    let path = settings_path(project);
    let scope = if project {
        super::provider_hooks::HookScope::Project
    } else {
        super::provider_hooks::HookScope::Global
    };
    let plans = super::provider_hooks::stage_provider_hooks(
        &[coding_brain_core::provider::AgentProvider::Codex],
        scope,
    )?;
    let changed = plans
        .iter()
        .flat_map(|plan| &plan.edits)
        .any(|edit| edit.original.as_deref() != Some(edit.replacement.as_slice()));
    let preserved = plans
        .iter()
        .any(|plan| !plan.preserved_modified_entries.is_empty());

    report_preserved_entries(&plans);

    if dry_run {
        if changed {
            println!(
                "Would update managed Coding Brain hooks in {}",
                path.display()
            );
        } else if preserved {
            println!(
                "No managed hook changes would be applied in {}; user-modified entries would be preserved",
                path.display()
            );
        } else {
            println!("Coding Brain hooks are current in {}", path.display());
        }
        return Ok(());
    }

    if !changed {
        if preserved {
            println!(
                "No managed hook changes applied in {}; user-modified entries were preserved",
                path.display()
            );
        } else {
            println!("Coding Brain hooks are current in {}", path.display());
        }
        return Ok(());
    }

    super::provider_hooks::apply_hook_transaction(&plans)?;
    print_success(&path);

    Ok(())
}

fn report_preserved_entries(plans: &[super::provider_hooks::ProviderHookPlan]) {
    for plan in plans {
        for entry in &plan.preserved_modified_entries {
            eprintln!(
                "Preserved user-modified {} hook entry: {entry}",
                plan.provider
            );
        }
    }
}

fn print_success(path: &Path) {
    println!("Initialized Coding Brain hooks in {}", path.display());
    println!();
    println!("Hooks installed:");
    println!("  Lifecycle events — keep Brain activity current");
    println!("  PermissionRequest (*) — observes every tool; brain decisions remain Bash-only");
    println!();
    println!("Restart Codex, then open `/hooks` to review and trust the command.");
    println!("Codex will then report lifecycle changes and ask the brain to review Bash requests.");
    println!("Run `coding-brain` to open the Brain TUI.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_codex_provider_dispatch_is_current() {
        let permission = serde_json::json!({
            "hooks": {"PermissionRequest": [{"matcher":"*","hooks":[{
                "type":"command",
                "command":"coding-brain --permission-hook --provider codex",
                "timeout":30,
                "statusMessage":"Brain reviewing permission…"
            }]}]}
        });
        let mut permission_scope = PermissionHookScope::default();
        inspect_permission_handlers(&permission, &mut permission_scope);
        assert!(permission_scope.current);

        let lifecycle = serde_json::json!({
            "hooks": {"Stop": [{"hooks":[{
                "type":"command",
                "command":"coding-brain --recovery-hook --provider codex",
                "timeout":30
            }]}]}
        });
        let mut lifecycle_scope = LifecycleHookScope::default();
        inspect_lifecycle_handlers(&lifecycle, &mut lifecycle_scope);
        assert!(lifecycle_scope.events[&ManagedHookEvent::Stop].current);
    }

    #[test]
    fn test_build_hooks_value() {
        let hooks = build_hooks_value();
        let obj = hooks.as_object().unwrap();
        let expected = [
            (
                "SessionStart",
                Some("startup|resume|clear|compact"),
                "--lifecycle-hook",
                2,
            ),
            ("UserPromptSubmit", None, "--lifecycle-hook", 2),
            ("PreToolUse", Some("*"), "--lifecycle-hook", 2),
            ("PermissionRequest", Some("*"), "--permission-hook", 30),
            ("PostToolUse", Some("*"), "--lifecycle-hook", 2),
            ("SubagentStart", Some("*"), "--lifecycle-hook", 2),
            ("SubagentStop", Some("*"), "--lifecycle-hook", 2),
            ("Stop", None, "--recovery-hook", 30),
        ];

        assert_eq!(obj.len(), expected.len());
        for (event, matcher, argument, timeout) in expected {
            let entry = &hooks[event][0];
            assert_eq!(
                entry.get("matcher").and_then(serde_json::Value::as_str),
                matcher
            );
            let handler = &entry["hooks"][0];
            assert_eq!(handler["command"], format!("coding-brain {argument}"));
            assert_eq!(handler["timeout"], timeout);
        }
    }

    #[test]
    fn failed_pre_rename_write_preserves_original_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        std::fs::write(&path, b"original\n").unwrap();
        std::fs::create_dir(path.with_extension("json.coding-brain.tmp")).unwrap();

        assert!(write_hooks_atomically(&path, b"replacement\n").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original\n");
    }

    #[test]
    fn test_has_codexctl_hooks_empty() {
        let settings = serde_json::json!({});
        assert!(!has_codexctl_hooks(&settings));
    }

    #[test]
    fn test_has_codexctl_hooks_present() {
        let settings = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });
        assert!(has_codexctl_hooks(&settings));
    }

    #[test]
    fn test_has_codexctl_hooks_other_hooks_only() {
        let settings = serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "echo hello",
                        "timeout": 5
                    }]
                }]
            }
        });
        assert!(!has_codexctl_hooks(&settings));
    }

    #[test]
    fn test_merge_hooks_empty() {
        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);

        assert!(settings.get("hooks").is_some());
        let hooks = settings["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 8);
        assert!(hooks.contains_key("PermissionRequest"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("Stop"));
    }

    #[test]
    fn test_merge_hooks_preserves_existing() {
        let mut settings = serde_json::json!({
            "allowedTools": ["Bash", "Read"],
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Write",
                    "hooks": [{
                        "type": "command",
                        "command": "echo validate-write",
                        "timeout": 10
                    }]
                }]
            }
        });

        merge_hooks(&mut settings);

        // Existing allowedTools preserved
        assert_eq!(
            settings["allowedTools"],
            serde_json::json!(["Bash", "Read"])
        );

        // Existing PermissionRequest Write hook preserved
        let pre = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(pre.len(), 2); // original Write + new wildcard
        assert_eq!(pre[0]["matcher"], "Write");
        assert_eq!(pre[1]["matcher"], "*");

        assert!(settings["hooks"].get("PostToolUse").is_some());
        assert!(settings["hooks"].get("Stop").is_some());
    }

    #[test]
    fn test_run_init_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings_file = dir.path().join(".codex/hooks.json");

        // Temporarily override HOME so settings_path uses our temp dir
        // We test the file-writing logic directly instead
        let parent = settings_file.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();

        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);

        let json = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&settings_file, format!("{json}\n")).unwrap();

        // Verify the file was created and is valid JSON
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("hooks").is_some());
        assert!(has_codexctl_hooks(&parsed));
    }

    #[test]
    fn test_settings_path_global() {
        let path = settings_path(false);
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with(".codex/hooks.json"));
    }

    #[test]
    fn test_settings_path_project() {
        let path = settings_path(true);
        assert_eq!(path, PathBuf::from(".codex/hooks.json"));
    }

    #[test]
    fn test_remove_codexctl_hooks_all() {
        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);
        assert!(has_codexctl_hooks(&settings));

        let removed = remove_codexctl_hooks(&mut settings);
        assert_eq!(removed, 8);
        assert!(!has_codexctl_hooks(&settings));
        // hooks key removed entirely when empty
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_remove_codexctl_hooks_preserves_others() {
        let mut settings = serde_json::json!({
            "allowedTools": ["Bash"],
            "hooks": {
                "PermissionRequest": [
                    {
                        "matcher": "Write",
                        "hooks": [{
                            "type": "command",
                            "command": "echo validate-write",
                            "timeout": 10
                        }]
                    },
                    {
                        "matcher": "*",
                        "hooks": [{
                            "type": "command",
                            "command": "codexctl --permission-hook",
                            "timeout": 30
                        }]
                    }
                ],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });

        let removed = remove_codexctl_hooks(&mut settings);
        assert_eq!(removed, 2); // PermissionRequest + legacy PostToolUse

        // Write hook in PermissionRequest preserved
        let pre = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], "Write");

        // PostToolUse event removed entirely (was only codexctl)
        assert!(settings["hooks"].get("PostToolUse").is_none());

        // allowedTools untouched
        assert_eq!(settings["allowedTools"], serde_json::json!(["Bash"]));
    }

    #[test]
    fn test_remove_codexctl_hooks_noop_when_absent() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "echo hello",
                        "timeout": 5
                    }]
                }]
            }
        });

        let removed = remove_codexctl_hooks(&mut settings);
        assert_eq!(removed, 0);
        // Original hook still present
        assert!(
            settings["hooks"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn test_remove_then_no_hooks_key() {
        // Settings that only had codexctl hooks — hooks key should be removed entirely
        let mut settings = serde_json::json!({ "permissions": {} });
        merge_hooks(&mut settings);
        remove_codexctl_hooks(&mut settings);

        assert!(settings.get("hooks").is_none());
        // Other keys preserved
        assert!(settings.get("permissions").is_some());
    }

    #[test]
    fn test_init_uninit_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let settings_file = dir.path().join("hooks.json");

        // Start with existing settings
        let original = serde_json::json!({
            "allowedTools": ["Read", "Glob"],
            "hooks": {
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "echo started",
                        "timeout": 5
                    }]
                }]
            }
        });
        let json = serde_json::to_string_pretty(&original).unwrap();
        std::fs::write(&settings_file, &json).unwrap();

        // Init: merge codexctl hooks in
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let mut settings: serde_json::Value = serde_json::from_str(&content).unwrap();
        merge_hooks(&mut settings);
        let json = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&settings_file, &json).unwrap();
        assert!(has_codexctl_hooks(&settings));

        // Uninit: remove codexctl hooks
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let mut settings: serde_json::Value = serde_json::from_str(&content).unwrap();
        remove_codexctl_hooks(&mut settings);

        // Back to original state
        assert!(!has_codexctl_hooks(&settings));
        assert_eq!(
            settings["allowedTools"],
            serde_json::json!(["Read", "Glob"])
        );
        let session_start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["hooks"][0]["command"], "echo started");
    }

    #[test]
    fn permission_hook_spec_uses_native_decision_adapter() {
        let hooks = build_hooks_value();
        assert_eq!(hooks["PermissionRequest"][0]["matcher"], "*");
        let handler = &hooks["PermissionRequest"][0]["hooks"][0];

        assert_eq!(handler["command"], "coding-brain --permission-hook");
        assert_eq!(handler["timeout"], 30);
        assert_eq!(handler["statusMessage"], "Brain reviewing permission…");
    }

    #[test]
    fn merge_preserves_unowned_legacy_permission_snapshot_and_is_idempotent() {
        let mut settings = serde_json::json!({
            "custom": { "keep": true },
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });

        merge_hooks(&mut settings);
        let once = settings.clone();
        merge_hooks(&mut settings);

        assert_eq!(settings, once);
        assert_eq!(settings["custom"], serde_json::json!({ "keep": true }));
        let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission.len(), 2);
        assert_eq!(
            permission[1]["hooks"][0]["command"],
            "coding-brain --permission-hook"
        );
    }

    #[test]
    fn merge_removes_legacy_lifecycle_hooks_and_preserves_external_stop() {
        let external_stop = serde_json::json!({
            "type": "command",
            "command": "/nix/store/test-codex-jj-stop-hook",
        });
        let mut settings = serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "/nix/store/test-codexctl/bin/codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }],
                "Stop": [{
                    "hooks": [
                        external_stop.clone(),
                        {
                            "type": "command",
                            "command": "codexctl --json",
                            "timeout": 5
                        }
                    ]
                }]
            }
        });

        merge_hooks(&mut settings);
        let once = settings.clone();
        merge_hooks(&mut settings);

        assert_eq!(settings, once);
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "coding-brain --lifecycle-hook"
        );
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"],
            serde_json::json!([external_stop])
        );
        assert_eq!(
            settings["hooks"]["Stop"][1]["hooks"][0]["command"],
            "coding-brain --recovery-hook"
        );
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            "coding-brain --permission-hook"
        );
    }

    #[test]
    fn merge_replaces_absolute_snapshot_without_shell_suffix_idempotently() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "/nix/store/test-codexctl/bin/codexctl --json",
                        "timeout": 5
                    }]
                }]
            }
        });

        assert!(has_codexctl_hooks(&settings));
        merge_hooks(&mut settings);
        let once = settings.clone();
        merge_hooks(&mut settings);

        assert_eq!(settings, once);
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "coding-brain --lifecycle-hook"
        );
    }

    #[test]
    fn merge_preserves_permission_hook_with_extra_arguments() {
        let custom = serde_json::json!({
            "type": "command",
            "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook --unexpected",
            "timeout": 45,
            "custom": "preserve"
        });
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [custom.clone()]
            }] }
        });

        merge_hooks(&mut settings);

        let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission.len(), 2);
        assert_eq!(permission[0]["hooks"], serde_json::json!([custom]));
        assert_eq!(
            permission[1]["hooks"][0]["command"],
            "coding-brain --permission-hook"
        );
    }

    #[test]
    fn merge_preserves_lookalike_permission_executable() {
        let lookalike = serde_json::json!({
            "type": "command",
            "command": "notify-codexctl --permission-hook",
            "timeout": 30
        });
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [lookalike.clone()]
            }] }
        });

        merge_hooks(&mut settings);

        let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission[0]["hooks"], serde_json::json!([lookalike]));
    }

    #[test]
    fn merge_preserves_relative_permission_and_snapshot_executables() {
        let relative_permission = serde_json::json!({
            "type": "command",
            "command": "./codexctl --permission-hook",
            "timeout": 30
        });
        let relative_snapshot = serde_json::json!({
            "type": "command",
            "command": "tools/codexctl --json",
            "timeout": 5
        });
        let mut settings = serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [relative_permission.clone()]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [relative_snapshot.clone()]
                }]
            }
        });

        merge_hooks(&mut settings);

        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"],
            serde_json::json!([relative_permission])
        );
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"],
            serde_json::json!([relative_snapshot])
        );
    }

    #[test]
    fn merge_preserves_unrelated_handlers_and_keys_in_shared_matcher() {
        let unrelated = serde_json::json!({
            "type": "command",
            "command": "notify-send codexctl",
            "timeout": 7,
            "custom": "unchanged"
        });
        let mut settings = serde_json::json!({
            "topLevel": [1, 2, 3],
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "*",
                    "customMatcherKey": "keep",
                    "hooks": [
                        unrelated.clone(),
                        {
                            "type": "command",
                            "command": "codexctl --permission-hook",
                            "timeout": 30
                        }
                    ]
                }]
            }
        });

        merge_hooks(&mut settings);

        assert_eq!(settings["topLevel"], serde_json::json!([1, 2, 3]));
        let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission.len(), 2);
        assert_eq!(permission[0]["customMatcherKey"], "keep");
        assert_eq!(permission[0]["hooks"], serde_json::json!([unrelated]));
    }

    fn write_hooks(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn lifecycle_discovery(
        hooks: serde_json::Value,
    ) -> (tempfile::TempDir, LifecycleHookDiscovery) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({ "hooks": hooks }),
        );
        let discovery = discover_lifecycle_hooks_at(Some(&home), &cwd);
        (temp, discovery)
    }

    #[test]
    fn lifecycle_discovery_reports_complete_current_definitions() {
        let (_, discovery) = lifecycle_discovery(build_hooks_value());

        assert!(discovery.global.definitions_current());
        assert!(!discovery.project.configured());
        assert!(discovery.trust_unverified);
    }

    #[test]
    fn lifecycle_discovery_reports_missing_stale_disabled_and_unavailable_events() {
        let mut missing = build_hooks_value();
        missing.as_object_mut().unwrap().remove("SessionStart");
        let (_, discovery) = lifecycle_discovery(missing);
        assert!(!discovery.global.events[&ManagedHookEvent::SessionStart].configured);

        let mut stale = build_hooks_value();
        stale["PostToolUse"][0]["hooks"][0]["timeout"] = serde_json::json!(7);
        let (_, discovery) = lifecycle_discovery(stale);
        assert!(discovery.global.events[&ManagedHookEvent::PostToolUse].stale);

        let mut bash_matcher = build_hooks_value();
        bash_matcher["PermissionRequest"][0]["matcher"] = serde_json::json!("Bash");
        let (_, discovery) = lifecycle_discovery(bash_matcher);
        assert!(discovery.global.events[&ManagedHookEvent::PermissionRequest].stale);

        let mut disabled = build_hooks_value();
        disabled["SubagentStart"][0]["hooks"][0]["enabled"] = serde_json::json!(false);
        let (_, discovery) = lifecycle_discovery(disabled);
        assert!(discovery.global.events[&ManagedHookEvent::SubagentStart].disabled);

        let mut unavailable = build_hooks_value();
        unavailable["Stop"][0]["hooks"][0]["command"] =
            serde_json::json!("/definitely/missing/codexctl --lifecycle-hook");
        let (_, discovery) = lifecycle_discovery(unavailable);
        assert!(discovery.global.events[&ManagedHookEvent::Stop].unavailable);
    }

    #[test]
    fn lifecycle_discovery_reports_duplicate_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let current = serde_json::json!({ "hooks": build_hooks_value() });
        write_hooks(&home.join(".codex/hooks.json"), current.clone());
        write_hooks(&cwd.join(".codex/hooks.json"), current);

        let discovery = discover_lifecycle_hooks_at(Some(&home), &cwd);

        assert!(discovery.duplicate_scopes());
    }

    #[test]
    fn uninit_removes_exact_lifecycle_handlers_and_preserves_lookalikes() {
        let lookalike = serde_json::json!({
            "type": "command",
            "command": "bin/codexctl --lifecycle-hook",
            "timeout": 2
        });
        let user_handler = serde_json::json!({
            "type": "command",
            "command": "echo validate",
            "timeout": 3
        });
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [
                        user_handler.clone(),
                        lookalike.clone(),
                        { "type": "command", "command": "/nix/store/test/bin/codexctl --lifecycle-hook", "timeout": 2 }
                    ]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{ "type": "command", "command": "codexctl --json", "timeout": 5 }]
                }],
                "Stop": [{
                    "hooks": [{ "type": "command", "command": "codexctl --lifecycle-hook", "timeout": 2 }]
                }]
            }
        });

        assert_eq!(remove_codexctl_hooks(&mut settings), 3);
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"],
            serde_json::json!([user_handler, lookalike])
        );
        assert!(settings["hooks"].get("PostToolUse").is_none());
        assert!(settings["hooks"].get("Stop").is_none());
    }

    #[test]
    fn discovery_treats_disabled_and_stale_managed_handlers_as_configured() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 5,
                        "statusMessage": "old copy",
                        "enabled": false
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.configured());
        assert!(discovery.global.configured);
        assert!(discovery.global.disabled);
        assert!(discovery.global.stale);
        assert!(!discovery.project.configured);
    }

    #[test]
    fn discovery_treats_absolute_permission_hook_as_current() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "/nix/store/test-coding-brain/bin/coding-brain --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.global.configured);
        assert!(discovery.global.current);
        assert!(!discovery.global.stale);
    }

    #[test]
    fn discovery_ignores_relative_permission_executable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "./codexctl --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(!discovery.global.configured);
        assert!(!discovery.global.current);
    }

    #[test]
    fn permission_hook_with_extra_arguments_is_managed_but_stale() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook --unexpected",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.global.configured);
        assert!(!discovery.global.current);
        assert!(discovery.global.stale);
    }

    #[test]
    fn discovery_ignores_legacy_permission_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(!discovery.configured());
        assert!(!discovery.global.stale);
        assert!(!discovery.global.current);
    }

    #[test]
    fn discovery_finds_applicable_project_only_handler() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("project");
        let cwd = root.join("nested/dir");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        write_hooks(
            &root.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "coding-brain --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.configured());
        assert!(!discovery.global.configured);
        assert!(discovery.project.configured);
        assert!(!discovery.project.stale);
    }

    #[test]
    fn discovery_reports_duplicate_global_and_project_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let current = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "codexctl --permission-hook",
                    "timeout": 30,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }] }
        });
        write_hooks(&home.join(".codex/hooks.json"), current.clone());
        write_hooks(&cwd.join(".codex/hooks.json"), current);

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.duplicate_scopes());
    }

    #[test]
    fn discovery_honors_custom_project_root_marker_from_user_config() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("project");
        let cwd = root.join("nested/dir");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "project_root_markers = [\".jj\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        // A nearer `.git` marker must not win when the user replaced the
        // default marker list with `.jj`.
        std::fs::create_dir_all(root.join("nested/.git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        write_hooks(
            &root.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.project.configured);
    }

    #[test]
    fn discovery_accepts_linked_worktree_git_file_as_root_marker() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("worktree");
        let cwd = root.join("nested");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /tmp/main/.git/worktrees/wt\n").unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        write_hooks(
            &root.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(discovery.project.configured);
    }

    #[test]
    fn empty_project_root_markers_limit_discovery_to_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("project");
        let cwd = root.join("nested");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "project_root_markers = []\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        write_hooks(
            &root.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 30,
                        "statusMessage": "Brain reviewing permission…"
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(!discovery.project.configured);
    }

    #[test]
    fn legacy_snapshot_in_write_matcher_is_user_owned() {
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Write",
                "hooks": [{
                    "type": "command",
                    "command": "codexctl --json",
                    "timeout": 5
                }]
            }] }
        });

        merge_hooks(&mut settings);

        let matchers = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(matchers.len(), 2);
        assert_eq!(matchers[0]["matcher"], "Write");
        assert_eq!(matchers[0]["hooks"][0]["command"], "codexctl --json");
    }

    #[test]
    fn discovery_ignores_legacy_snapshot_outside_bash_matcher() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            serde_json::json!({
                "hooks": { "PermissionRequest": [{
                    "matcher": "Write",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json",
                        "timeout": 5
                    }]
                }] }
            }),
        );

        let discovery = discover_permission_hooks_at(Some(&home), &cwd);

        assert!(!discovery.configured());
    }

    #[test]
    fn uninit_preserves_user_owned_write_snapshot_handler() {
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [
                {
                    "matcher": "Write",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --json",
                        "timeout": 5
                    }]
                },
                {
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 30
                    }]
                }
            ] }
        });

        let removed = remove_codexctl_hooks(&mut settings);

        assert_eq!(removed, 1);
        let matchers = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0]["matcher"], "Write");
    }

    #[test]
    fn uninit_counts_managed_handler_removed_from_shared_matcher() {
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [
                    {
                        "type": "command",
                        "command": "echo keep",
                        "timeout": 5
                    },
                    {
                        "type": "command",
                        "command": "codexctl --permission-hook",
                        "timeout": 30
                    }
                ]
            }] }
        });

        let removed = remove_codexctl_hooks(&mut settings);

        assert_eq!(removed, 1);
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            "echo keep"
        );
    }

    #[test]
    fn uninit_preserves_permission_hook_with_extra_arguments() {
        let mut settings = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook --unexpected",
                    "timeout": 45,
                    "custom": "preserve"
                }]
            }] }
        });
        let original = settings.clone();

        let removed = remove_codexctl_hooks(&mut settings);

        assert_eq!(removed, 0);
        assert_eq!(settings, original);
    }

    #[test]
    fn uninit_preserves_relative_permission_and_snapshot_executables() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "tools/codexctl --permission-hook",
                        "timeout": 30
                    }]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "./codexctl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });
        let original = settings.clone();

        let removed = remove_codexctl_hooks(&mut settings);

        assert_eq!(removed, 0);
        assert_eq!(settings, original);
    }
}
