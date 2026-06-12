use std::process::Command;

use serde_json::Value;

use super::{FetchResult, LoopSource, SourceItem};
use crate::r#loop::LoopResult;

pub struct ShellSource {
    name: String,
    command: String,
    default_limit: usize,
}

impl ShellSource {
    pub fn new(name: &str, command: String, default_limit: usize) -> Self {
        Self {
            name: name.into(),
            command,
            default_limit,
        }
    }
}

impl LoopSource for ShellSource {
    fn source_key(&self) -> String {
        format!("shell:{}", self.name)
    }

    fn fetch(&self, _cursor: Option<Value>, limit: usize) -> LoopResult<FetchResult> {
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&self.command)
            .output()
            .map_err(|e| format!("run shell source: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "shell source exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let limit = limit.min(self.default_limit);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut items = Vec::new();
        for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
            if items.len() >= limit {
                break;
            }
            let value: Value =
                serde_json::from_str(line).map_err(|e| format!("shell source JSON line: {e}"))?;
            let id = value
                .get("id")
                .and_then(value_to_id)
                .ok_or_else(|| "shell source item missing id".to_string())?;
            let title = value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let body = value
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
            items.push(SourceItem {
                source_kind: "shell".into(),
                source_item_id: format!("shell:{}:{id}", self.name),
                title,
                body,
                url,
                raw_json: value,
            });
        }
        Ok(FetchResult {
            items,
            next_cursor: None,
        })
    }
}

fn value_to_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::r#loop::sources::LoopSource;

    #[test]
    fn shell_source_reads_json_lines() {
        let source = ShellSource::new(
            "test",
            "printf '{\"id\":\"one\",\"title\":\"One\",\"body\":\"Body\"}\\n'".into(),
            10,
        );

        let fetched = source.fetch(None, 10).unwrap();

        assert_eq!(fetched.items.len(), 1);
        assert_eq!(fetched.items[0].source_item_id, "shell:test:one");
    }
}
