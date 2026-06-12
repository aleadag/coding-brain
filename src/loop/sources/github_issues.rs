use std::process::Command;

use serde_json::Value;

use super::{FetchResult, LoopSource, SourceItem};
use crate::r#loop::LoopResult;

pub struct GithubIssuesSource {
    repo: String,
    query: Option<String>,
    default_limit: usize,
}

impl GithubIssuesSource {
    pub fn new(repo: String, query: Option<String>, default_limit: usize) -> Self {
        Self {
            repo,
            query,
            default_limit,
        }
    }
}

impl LoopSource for GithubIssuesSource {
    fn source_key(&self) -> String {
        format!("github_issues:{}", self.repo)
    }

    fn fetch(&self, _cursor: Option<Value>, limit: usize) -> LoopResult<FetchResult> {
        let limit = limit.min(self.default_limit).to_string();
        let mut cmd = Command::new("gh");
        cmd.args([
            "issue",
            "list",
            "--repo",
            &self.repo,
            "--limit",
            &limit,
            "--json",
            "number,title,body,url,labels,updatedAt",
        ]);
        if let Some(query) = &self.query {
            cmd.args(["--search", query]);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("run gh issue list: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh issue list exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let values: Vec<Value> =
            serde_json::from_str(&stdout).map_err(|e| format!("parse gh issue JSON: {e}"))?;
        let mut items = Vec::new();
        for value in values {
            let number = value
                .get("number")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "GitHub issue missing number".to_string())?;
            let title = value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
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
                source_kind: "github_issues".into(),
                source_item_id: format!("github:{}#{number}", self.repo),
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
