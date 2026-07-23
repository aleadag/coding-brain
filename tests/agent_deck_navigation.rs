#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

use coding_brain::runtime::LiveSessionNavigation;
use coding_brain_core::brain_activity::SessionTarget;
use coding_brain_core::project::ProjectId;
use coding_brain_tui::terminal_suspend::{NavigationOutcome, TerminalControl, navigate_to_session};

#[test]
fn fake_agent_deck_resolves_attaches_and_restores_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("agent-deck");
    let invocations = directory.path().join("invocations");
    fs::write(
        &executable,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1" = "list" ]; then
  printf '%s' '[{{"id":"deck-1","title":"project","path":"/work/project","future":true}}]'
  exit 0
fi
if [ "$1" = "session" ] && [ "$2" = "attach" ] && [ "$3" = "deck-1" ]; then
  exit 0
fi
exit 9
"#,
            invocations.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let terminal = FakeTerminal::default();
    let navigation = LiveSessionNavigation::new(executable);

    let outcome = navigate_to_session(&terminal, &navigation, &target()).unwrap();

    assert_eq!(outcome, NavigationOutcome::Attached);
    assert_eq!(
        fs::read_to_string(invocations)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["list --json", "session attach deck-1"]
    );
    assert_eq!(
        terminal.calls.lock().unwrap().as_slice(),
        [
            "show_cursor",
            "leave_alt",
            "raw_off",
            "raw_on",
            "enter_alt",
            "hide_cursor",
            "redraw",
        ]
    );
}

fn target() -> SessionTarget {
    SessionTarget {
        provider: coding_brain_core::provider::AgentProvider::Codex,
        session_id: "deck-1".into(),
        turn_id: None,
        tool_use_id: None,
        project_id: ProjectId::Stable("project-1".into()),
        cwd: PathBuf::from("/work/project"),
        provider_hints: Vec::new(),
    }
}

#[derive(Default)]
struct FakeTerminal {
    calls: Mutex<Vec<&'static str>>,
}

impl FakeTerminal {
    fn record(&self, call: &'static str) -> io::Result<()> {
        self.calls.lock().unwrap().push(call);
        Ok(())
    }
}

impl TerminalControl for FakeTerminal {
    fn show_cursor(&self) -> io::Result<()> {
        self.record("show_cursor")
    }

    fn leave_alternate_screen(&self) -> io::Result<()> {
        self.record("leave_alt")
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        self.record("raw_off")
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        self.record("raw_on")
    }

    fn enter_alternate_screen(&self) -> io::Result<()> {
        self.record("enter_alt")
    }

    fn hide_cursor(&self) -> io::Result<()> {
        self.record("hide_cursor")
    }

    fn redraw(&self) -> io::Result<()> {
        self.record("redraw")
    }
}
