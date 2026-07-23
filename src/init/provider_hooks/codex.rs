use super::{HookDefinition, merge_nested_hooks};
use serde_json::Value;
use std::io;

const DEFINITIONS: &[HookDefinition] = &[
    HookDefinition::nested(
        "SessionStart",
        Some("startup|resume|clear|compact"),
        "--lifecycle-hook",
        2,
    ),
    HookDefinition::nested("UserPromptSubmit", None, "--lifecycle-hook", 2),
    HookDefinition::nested("PreToolUse", Some("*"), "--lifecycle-hook", 2),
    HookDefinition::permission("PermissionRequest", "--permission-hook"),
    HookDefinition::nested("PostToolUse", Some("*"), "--lifecycle-hook", 2),
    HookDefinition::nested("SubagentStart", Some("*"), "--lifecycle-hook", 2),
    HookDefinition::nested("SubagentStop", Some("*"), "--lifecycle-hook", 2),
    HookDefinition::nested("Stop", None, "--recovery-hook", 30),
];

pub(super) fn merge(root: &mut Value, remove: bool, preserved: &mut Vec<String>) -> io::Result<()> {
    merge_nested_hooks(root, "codex", DEFINITIONS, remove, true, preserved)
}
