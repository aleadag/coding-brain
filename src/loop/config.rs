#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::LoopResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Report,
    Assisted,
    Unattended,
}

impl LoopMode {
    pub fn parse(value: &str) -> LoopResult<Self> {
        match value {
            "report" => Ok(Self::Report),
            "assisted" => Ok(Self::Assisted),
            "unattended" => Ok(Self::Unattended),
            other => Err(format!("unknown loop mode {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Shell,
    GithubIssues,
}

impl SourceKind {
    fn parse(value: &str) -> LoopResult<Self> {
        match value {
            "shell" => Ok(Self::Shell),
            "github_issues" => Ok(Self::GithubIssues),
            other => Err(format!("unknown source kind {other}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::GithubIssues => "github_issues",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageMode {
    Deterministic,
    Model,
}

impl TriageMode {
    fn parse(value: &str) -> LoopResult<Self> {
        match value {
            "deterministic" => Ok(Self::Deterministic),
            "model" => Ok(Self::Model),
            other => Err(format!("unknown triage mode {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeMode {
    None,
    Existing,
    Required,
    Auto,
}

impl WorktreeMode {
    pub fn parse(value: &str) -> LoopResult<Self> {
        match value {
            "none" => Ok(Self::None),
            "existing" => Ok(Self::Existing),
            "required" => Ok(Self::Required),
            "auto" => Ok(Self::Auto),
            other => Err(format!("unknown worktree mode {other}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Existing => "existing",
            Self::Required => "required",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub name: String,
    pub enabled: bool,
    pub mode: LoopMode,
    pub cadence: Option<String>,
    pub path: PathBuf,
    pub source: SourceConfig,
    pub triage: TriageConfig,
    pub execution: ExecutionConfig,
    pub verify: Vec<VerifierConfig>,
    pub gates: GateConfig,
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub kind: SourceKind,
    pub repo: Option<String>,
    pub query: Option<String>,
    pub command: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct TriageConfig {
    pub mode: TriageMode,
    pub skill: Option<String>,
    pub instructions: Option<String>,
    pub allowed_actions: Vec<String>,
    pub allowed_worktree: Vec<WorktreeMode>,
    pub allowed_verifiers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub cwd: String,
    pub worktree: WorktreeMode,
    pub worktree_root: Option<String>,
    pub branch_template: Option<String>,
    pub session: String,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub max_retries: Option<u32>,
    pub timeout_min: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct VerifierConfig {
    pub kind: String,
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct GateConfig {
    pub max_items_per_run: usize,
    pub max_concurrent: usize,
    pub require_human_for: Vec<String>,
}

impl LoopConfig {
    pub fn validate_with_skills(&self, available_skills: &HashSet<String>) -> LoopResult<()> {
        if self.name.trim().is_empty() {
            return Err("loop name is required".into());
        }
        if let Some(skill) = self.triage.skill.as_deref() {
            if !available_skills.contains(skill) {
                return Err(format!("required skill {skill} not found"));
            }
        }
        if self.source.kind == SourceKind::Shell && self.source.command.is_none() {
            return Err("shell source requires source.command".into());
        }
        if self.source.kind == SourceKind::GithubIssues && self.source.repo.is_none() {
            return Err("github_issues source requires source.repo".into());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn minimal_for_test(name: &str) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            mode: LoopMode::Assisted,
            cadence: None,
            path: PathBuf::from(format!(".codexctl/loops/{name}.toml")),
            source: SourceConfig {
                kind: SourceKind::Shell,
                repo: None,
                query: None,
                command: Some("printf '{}\\n'".into()),
                limit: 10,
            },
            triage: TriageConfig {
                mode: TriageMode::Deterministic,
                skill: Some("loop-triage".into()),
                instructions: None,
                allowed_actions: vec![
                    "ignore".into(),
                    "report".into(),
                    "submit".into(),
                    "escalate".into(),
                ],
                allowed_worktree: vec![WorktreeMode::None, WorktreeMode::Required],
                allowed_verifiers: vec!["cargo test".into(), "cargo clippy -- -D warnings".into()],
            },
            execution: ExecutionConfig {
                cwd: ".".into(),
                worktree: WorktreeMode::None,
                worktree_root: None,
                branch_template: None,
                session: "headless".into(),
                model: None,
                budget_usd: None,
                max_retries: Some(2),
                timeout_min: Some(90),
            },
            verify: vec![VerifierConfig {
                kind: "run".into(),
                command: "cargo test".into(),
            }],
            gates: GateConfig {
                max_items_per_run: 2,
                max_concurrent: 1,
                require_human_for: Vec::new(),
            },
        }
    }
}

pub fn discover_project_loops(root: &Path) -> LoopResult<Vec<LoopConfig>> {
    let dir = root.join(".codexctl/loops");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut loops = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let body = fs::read_to_string(&path)
            .map_err(|e| format!("read loop config {}: {e}", path.display()))?;
        loops.push(parse_loop_config(&body, path)?);
    }
    loops.sort_by_key(|cfg| cfg.name.clone());
    Ok(loops)
}

pub fn parse_loop_config(body: &str, path: PathBuf) -> LoopResult<LoopConfig> {
    let table = parse_toml_subset(body)?;
    let root = table.get("").cloned().unwrap_or_default();
    let source = table.get("source").cloned().unwrap_or_default();
    let triage = table.get("triage").cloned().unwrap_or_default();
    let triage_allowed = table.get("triage.allowed").cloned().unwrap_or_default();
    let execution = table.get("execution").cloned().unwrap_or_default();
    let gates = table.get("gates").cloned().unwrap_or_default();
    let verify_sections = collect_array_sections(&table, "verify");

    let name = required(&root, "name")?;
    let source_kind = SourceKind::parse(&required(&source, "kind")?)?;
    let verify = verify_sections
        .iter()
        .filter_map(|section| {
            Some(VerifierConfig {
                kind: section.get("kind")?.clone(),
                command: section.get("command")?.clone(),
            })
        })
        .collect();

    Ok(LoopConfig {
        name,
        enabled: optional_bool(&root, "enabled").unwrap_or(true),
        mode: LoopMode::parse(root.get("mode").map(String::as_str).unwrap_or("assisted"))?,
        cadence: root.get("cadence").cloned(),
        path,
        source: SourceConfig {
            kind: source_kind,
            repo: source.get("repo").cloned(),
            query: source.get("query").cloned(),
            command: source.get("command").cloned(),
            limit: optional_usize(&source, "limit").unwrap_or(10),
        },
        triage: TriageConfig {
            mode: TriageMode::parse(
                triage
                    .get("mode")
                    .map(String::as_str)
                    .unwrap_or("deterministic"),
            )?,
            skill: triage.get("skill").cloned(),
            instructions: triage.get("instructions").cloned(),
            allowed_actions: optional_array(&triage_allowed, "actions").unwrap_or_else(|| {
                vec![
                    "ignore".into(),
                    "report".into(),
                    "submit".into(),
                    "escalate".into(),
                ]
            }),
            allowed_worktree: optional_array(&triage_allowed, "worktree")
                .unwrap_or_else(|| vec!["none".into(), "existing".into(), "required".into()])
                .iter()
                .map(|value| WorktreeMode::parse(value))
                .collect::<Result<Vec<_>, _>>()?,
            allowed_verifiers: optional_array(&triage_allowed, "verifiers").unwrap_or_default(),
        },
        execution: ExecutionConfig {
            cwd: execution.get("cwd").cloned().unwrap_or_else(|| ".".into()),
            worktree: WorktreeMode::parse(
                execution
                    .get("worktree")
                    .map(String::as_str)
                    .unwrap_or("existing"),
            )?,
            worktree_root: execution.get("worktree_root").cloned(),
            branch_template: execution.get("branch_template").cloned(),
            session: execution
                .get("session")
                .cloned()
                .unwrap_or_else(|| "headless".into()),
            model: execution.get("model").cloned(),
            budget_usd: optional_f64(&execution, "budget_usd"),
            max_retries: optional_u32(&execution, "max_retries"),
            timeout_min: optional_u32(&execution, "timeout_min"),
        },
        verify,
        gates: GateConfig {
            max_items_per_run: optional_usize(&gates, "max_items_per_run").unwrap_or(1),
            max_concurrent: optional_usize(&gates, "max_concurrent").unwrap_or(1),
            require_human_for: optional_array(&gates, "require_human_for").unwrap_or_default(),
        },
    })
}

type SectionMap = HashMap<String, HashMap<String, String>>;

fn parse_toml_subset(body: &str) -> LoopResult<SectionMap> {
    let mut out: SectionMap = HashMap::new();
    let mut section = String::new();
    let mut array_counts: HashMap<String, usize> = HashMap::new();
    let mut lines = body.lines().peekable();

    while let Some(raw) = lines.next() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("[[") && line.ends_with("]]") {
            let base = line.trim_matches(&['[', ']'][..]).to_string();
            let count = array_counts.entry(base.clone()).or_insert(0);
            section = format!("{base}#{count}");
            *count += 1;
            out.entry(section.clone()).or_default();
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(&['[', ']'][..]).to_string();
            out.entry(section.clone()).or_default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("unparseable loop config line: {line}"));
        };
        let key = key.trim().to_string();
        let mut value = value.trim().to_string();
        if value.starts_with("\"\"\"") {
            value = value.trim_start_matches("\"\"\"").to_string();
            let mut parts = Vec::new();
            if !value.is_empty() {
                parts.push(value);
            }
            loop {
                let Some(next) = lines.next() else {
                    return Err(format!("unterminated multiline string for {key}"));
                };
                if let Some((before, _)) = next.split_once("\"\"\"") {
                    parts.push(before.to_string());
                    break;
                }
                parts.push(next.to_string());
            }
            value = parts.join("\n").trim().to_string();
        } else {
            value = value.trim_matches('"').to_string();
        }
        out.entry(section.clone()).or_default().insert(key, value);
    }

    Ok(out)
}

fn collect_array_sections(table: &SectionMap, base: &str) -> Vec<HashMap<String, String>> {
    let mut rows: Vec<(usize, HashMap<String, String>)> = table
        .iter()
        .filter_map(|(name, values)| {
            let (prefix, idx) = name.split_once('#')?;
            if prefix != base {
                return None;
            }
            Some((idx.parse::<usize>().ok()?, values.clone()))
        })
        .collect();
    rows.sort_by_key(|(idx, _)| *idx);
    rows.into_iter().map(|(_, values)| values).collect()
}

fn required(map: &HashMap<String, String>, key: &str) -> LoopResult<String> {
    map.get(key)
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing required key {key}"))
}

fn optional_bool(map: &HashMap<String, String>, key: &str) -> Option<bool> {
    map.get(key).and_then(|v| v.parse::<bool>().ok())
}

fn optional_usize(map: &HashMap<String, String>, key: &str) -> Option<usize> {
    map.get(key).and_then(|v| v.parse::<usize>().ok())
}

fn optional_u32(map: &HashMap<String, String>, key: &str) -> Option<u32> {
    map.get(key).and_then(|v| v.parse::<u32>().ok())
}

fn optional_f64(map: &HashMap<String, String>, key: &str) -> Option<f64> {
    map.get(key).and_then(|v| v.parse::<f64>().ok())
}

fn optional_array(map: &HashMap<String, String>, key: &str) -> Option<Vec<String>> {
    let raw = map.get(key)?;
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    Some(
        trimmed
            .split(',')
            .map(|part| part.trim().trim_matches('"').to_string())
            .filter(|part| !part.is_empty())
            .collect(),
    )
}

fn strip_comment(raw: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &raw[..idx],
            _ => {}
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_loop_config_accepts_minimal_shell_loop() {
        let body = r#"
name = "daily-email"
enabled = true
mode = "report"
cadence = "1d"

[source]
kind = "shell"
command = "printf '{}\n'"
limit = 3

[triage]
mode = "deterministic"
skill = "loop-triage"

[execution]
cwd = "."
worktree = "none"
session = "headless"

[gates]
max_items_per_run = 2
max_concurrent = 1
"#;

        let cfg = parse_loop_config(
            body,
            std::path::PathBuf::from(".codexctl/loops/daily-email.toml"),
        )
        .unwrap();

        assert_eq!(cfg.name, "daily-email");
        assert_eq!(cfg.mode, LoopMode::Report);
        assert_eq!(cfg.source.kind, SourceKind::Shell);
        assert_eq!(cfg.source.command.as_deref(), Some("printf '{}\\n'"));
        assert_eq!(cfg.execution.worktree, WorktreeMode::None);
        assert_eq!(cfg.gates.max_items_per_run, 2);
    }

    #[test]
    fn parse_loop_config_keeps_hash_inside_quoted_values() {
        let body = r#"
name = "issue-triage"

[source]
kind = "github_issues"
repo = "aleadag/codexctl"
query = "is:open #loop"

[execution]
cwd = "."
"#;

        let cfg = parse_loop_config(
            body,
            std::path::PathBuf::from(".codexctl/loops/issue-triage.toml"),
        )
        .unwrap();

        assert_eq!(cfg.source.query.as_deref(), Some("is:open #loop"));
    }

    #[test]
    fn validate_loop_config_rejects_missing_skill() {
        let mut cfg = LoopConfig::minimal_for_test("issue-triage");
        cfg.triage.skill = Some("missing-skill".into());
        let available = std::collections::HashSet::from(["loop-triage".to_string()]);

        let err = cfg.validate_with_skills(&available).unwrap_err();

        assert!(err.contains("required skill missing-skill not found"));
    }
}
