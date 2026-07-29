use serde_json::{Value, json};
use std::io;

use crate::executable::{CURRENT_PROGRAM, STALE_MANAGED_PROGRAMS};

const MANAGED_NAME: &str = "coding-brain";

pub(super) fn merge(root: &mut Value, remove: bool, preserved: &mut Vec<String>) -> io::Result<()> {
    let object = root.as_object_mut().expect("root validated as object");
    if object.values().any(|definition| !definition.is_object()) {
        return Err(io::Error::other(
            "Antigravity hook definitions must be JSON objects",
        ));
    }
    let mut modified = false;
    if let Some(existing) = object.get(MANAGED_NAME) {
        if is_exact_managed_definition(existing) {
            object.remove(MANAGED_NAME);
        } else {
            preserved.push("antigravity:coding-brain".to_owned());
            modified = true;
        }
    }
    if !remove && !modified {
        object.insert(MANAGED_NAME.to_owned(), definition());
    }
    Ok(())
}

fn definition() -> Value {
    definition_for(CURRENT_PROGRAM)
}

fn definition_for(program: &str) -> Value {
    json!({
        "PreToolUse": [{"matcher": "*", "hooks": [{
            "type": "command", "command": format!("{program} --permission-hook --provider antigravity --antigravity-hook-event PreToolUse"), "timeout": 30
        }]}],
        "PostToolUse": [{"matcher": "*", "hooks": [{
            "type": "command", "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse"), "timeout": 2
        }]}],
        "PreInvocation": [{
            "type": "command", "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation"), "timeout": 2
        }],
        "PostInvocation": [{
            "type": "command", "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation"), "timeout": 2
        }],
        "Stop": [{
            "type": "command", "command": format!("{program} --recovery-hook --provider antigravity --antigravity-hook-event Stop"), "timeout": 30
        }]
    })
}

fn is_exact_managed_definition(existing: &Value) -> bool {
    existing == &definition()
        || STALE_MANAGED_PROGRAMS
            .iter()
            .any(|program| existing == &definition_for(program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_replaces_exact_current_and_stale_definitions() {
        for program in ["cbrain", "coding-brain", "codexctl"] {
            let mut root = json!({MANAGED_NAME: definition_for(program)});
            let mut preserved = Vec::new();

            merge(&mut root, false, &mut preserved).unwrap();

            assert_eq!(root[MANAGED_NAME], definition_for("cbrain"));
            assert!(preserved.is_empty());
        }
    }

    #[test]
    fn merge_preserves_lookalike_definitions_as_modified() {
        for program in ["coding-brain-wrapper", "my-codexctl"] {
            let original = definition_for(program);
            let mut root = json!({MANAGED_NAME: original.clone()});
            let mut preserved = Vec::new();

            merge(&mut root, false, &mut preserved).unwrap();

            assert_eq!(root[MANAGED_NAME], original);
            assert_eq!(preserved, vec!["antigravity:coding-brain"]);
        }
    }
}
