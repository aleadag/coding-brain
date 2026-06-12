#![allow(dead_code)]

pub mod github_issues;
pub mod shell;

use serde_json::Value;

use super::LoopResult;
use super::config::{LoopConfig, SourceKind};
use github_issues::GithubIssuesSource;
use shell::ShellSource;

#[derive(Debug, Clone)]
pub struct FetchResult {
    pub items: Vec<SourceItem>,
    pub next_cursor: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct SourceItem {
    pub source_kind: String,
    pub source_item_id: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub raw_json: Value,
}

impl SourceItem {
    pub fn summary(&self) -> String {
        if self.body.len() > 2000 {
            format!("{}...", &self.body[..2000])
        } else {
            self.body.clone()
        }
    }

    #[cfg(test)]
    pub fn for_test(source_item_id: &str) -> Self {
        Self {
            source_kind: "test".into(),
            source_item_id: source_item_id.into(),
            title: "Test item".into(),
            body: "Test body".into(),
            url: None,
            raw_json: serde_json::json!({"id": source_item_id}),
        }
    }
}

pub trait LoopSource {
    fn source_key(&self) -> String;
    fn fetch(&self, cursor: Option<Value>, limit: usize) -> LoopResult<FetchResult>;
}

pub fn source_from_config(cfg: &LoopConfig) -> LoopResult<Box<dyn LoopSource>> {
    match cfg.source.kind {
        SourceKind::Shell => {
            let command = cfg
                .source
                .command
                .clone()
                .ok_or_else(|| "shell source requires command".to_string())?;
            Ok(Box::new(ShellSource::new(
                &cfg.name,
                command,
                cfg.source.limit,
            )))
        }
        SourceKind::GithubIssues => {
            let repo = cfg
                .source
                .repo
                .clone()
                .ok_or_else(|| "github_issues source requires repo".to_string())?;
            Ok(Box::new(GithubIssuesSource::new(
                repo,
                cfg.source.query.clone(),
                cfg.source.limit,
            )))
        }
    }
}
