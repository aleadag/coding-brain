//! Minimal interactive-prompt helpers for the `coding-brain init` wizard.
//!
//! Intentionally thin: stdin/stdout only, no TUI library. Every prompt has a
//! default so a user can mash enter through the wizard and land at sensible
//! values. Non-interactive callers bypass these entirely.

use std::io::{self, BufRead, Write};

use coding_brain_core::provider::AgentProvider;

/// Yes/no question. `default = true` means hitting enter answers yes.
pub fn yes_no(prompt: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {hint} ");
    io::stdout().flush()?;
    let line = read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    Ok(matches!(trimmed.chars().next(), Some('y' | 'Y')))
}

/// Free-form line with an optional default. Returns `Some(default)` on empty
/// input when a default is provided, `None` when the user explicitly clears.
pub fn line_or_default(prompt: &str, default: Option<&str>) -> io::Result<Option<String>> {
    match default {
        Some(d) => print!("{prompt} [{d}]: "),
        None => print!("{prompt}: "),
    }
    io::stdout().flush()?;
    let line = read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default.map(String::from));
    }
    Ok(Some(trimmed.to_string()))
}

fn read_line() -> io::Result<String> {
    let mut buf = String::new();
    if io::stdin().lock().read_line(&mut buf)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "provider selection requires interactive input",
        ));
    }
    Ok(buf)
}

pub fn select_providers(detected: &[AgentProvider]) -> io::Result<Vec<AgentProvider>> {
    println!();
    println!("Select providers to configure (comma-separated):");
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        let marker = if detected.contains(&provider) {
            "x"
        } else {
            " "
        };
        let detail = if detected.contains(&provider) {
            "detected"
        } else {
            "not detected; may be installed later"
        };
        println!("  [{marker}] {} ({detail})", provider.as_str());
    }

    loop {
        let default = (!detected.is_empty()).then(|| {
            detected
                .iter()
                .map(|provider| provider.as_str())
                .collect::<Vec<_>>()
                .join(",")
        });
        let input = line_or_default("  Providers", default.as_deref())?.unwrap_or_default();
        match parse_provider_selection(&input, detected) {
            Ok(providers) => return Ok(providers),
            Err(message) => eprintln!("  {message}"),
        }
    }
}

fn parse_provider_selection(
    input: &str,
    detected: &[AgentProvider],
) -> Result<Vec<AgentProvider>, String> {
    let input = input.trim();
    if input.is_empty() {
        if detected.is_empty() {
            return Err("select at least one provider".into());
        }
        return Ok(canonicalize_providers(detected));
    }

    let values = input
        .split([',', ' '])
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.iter().any(|value| value.eq_ignore_ascii_case("all")) {
        if values
            .iter()
            .any(|value| !value.eq_ignore_ascii_case("all"))
        {
            return Err("`all` cannot be combined with another provider selector".into());
        }
        return Ok(vec![
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ]);
    }

    let mut selected = Vec::new();
    for value in values {
        let provider = match value.to_ascii_lowercase().as_str() {
            "codex" => AgentProvider::Codex,
            "claude" => AgentProvider::Claude,
            "antigravity" => AgentProvider::Antigravity,
            _ => return Err(format!("unknown provider `{value}`")),
        };
        selected.push(provider);
    }
    if selected.is_empty() {
        return Err("select at least one provider".into());
    }
    Ok(canonicalize_providers(&selected))
}

fn canonicalize_providers(values: &[AgentProvider]) -> Vec<AgentProvider> {
    [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ]
    .into_iter()
    .filter(|provider| values.contains(provider))
    .collect()
}

/// Print a section header. Used between phases so the wizard reads as a
/// numbered checklist rather than one wall of prompts.
pub fn section_header(idx: usize, total: usize, title: &str) {
    println!();
    println!("─── ({idx}/{total}) {title} ─────────────────────────────");
}

/// Print a small status block for a single phase's outcome.
pub fn phase_outcome(label: &str, summary: &str) {
    println!("  ✓ {label}: {summary}");
}

/// Print a skipped phase.
pub fn phase_skipped(label: &str, reason: &str) {
    println!("  — {label}: skipped ({reason})");
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_brain_core::provider::AgentProvider;

    #[test]
    fn provider_selection_defaults_to_detected_but_allows_installed_later_choices() {
        let detected = vec![AgentProvider::Codex];
        assert_eq!(
            parse_provider_selection("", &detected).unwrap(),
            vec![AgentProvider::Codex]
        );
        assert_eq!(
            parse_provider_selection("claude, antigravity", &detected).unwrap(),
            vec![AgentProvider::Claude, AgentProvider::Antigravity]
        );
    }

    #[test]
    fn provider_selection_requires_at_least_one_choice() {
        assert!(parse_provider_selection("", &[]).is_err());
        assert!(parse_provider_selection("none", &[AgentProvider::Codex]).is_err());
        assert!(parse_provider_selection("all,claude", &[]).is_err());
        assert_eq!(
            parse_provider_selection("all,all", &[]).unwrap(),
            vec![
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ]
        );
    }
}
