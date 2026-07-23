use serde_json::{Value, json};
use std::io;

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
        if existing == &definition() {
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
    json!({
        "PreToolUse": [{"matcher": "*", "hooks": [{
            "type": "command", "command": "coding-brain --permission-hook --provider antigravity --antigravity-hook-event PreToolUse", "timeout": 30
        }]}],
        "PostToolUse": [{"matcher": "*", "hooks": [{
            "type": "command", "command": "coding-brain --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse", "timeout": 2
        }]}],
        "PreInvocation": [{
            "type": "command", "command": "coding-brain --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation", "timeout": 2
        }],
        "PostInvocation": [{
            "type": "command", "command": "coding-brain --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation", "timeout": 2
        }],
        "Stop": [{
            "type": "command", "command": "coding-brain --recovery-hook --provider antigravity --antigravity-hook-event Stop", "timeout": 30
        }]
    })
}
