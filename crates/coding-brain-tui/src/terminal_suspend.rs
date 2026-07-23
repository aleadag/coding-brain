use std::io;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use coding_brain_core::brain_activity::SessionTarget;
use coding_brain_core::runtime::{ExternalCommand, NavigationPlan, SessionNavigation};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationOutcome {
    Attached,
    Cancelled { restore_error: Option<String> },
    FocusedFallback,
}

pub trait TerminalControl: Send + Sync {
    fn show_cursor(&self) -> io::Result<()>;
    fn leave_alternate_screen(&self) -> io::Result<()>;
    fn disable_raw_mode(&self) -> io::Result<()>;
    fn enable_raw_mode(&self) -> io::Result<()>;
    fn enter_alternate_screen(&self) -> io::Result<()>;
    fn hide_cursor(&self) -> io::Result<()>;
    fn redraw(&self) -> io::Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermTerminalControl;

impl TerminalControl for CrosstermTerminalControl {
    fn show_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn leave_alternate_screen(&self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate_screen(&self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn hide_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Hide)
    }

    fn redraw(&self) -> io::Result<()> {
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))
    }
}

pub struct TerminalSuspendGuard<'a> {
    terminal: &'a dyn TerminalControl,
    restored: bool,
}

impl<'a> TerminalSuspendGuard<'a> {
    pub fn suspend(terminal: &'a dyn TerminalControl) -> io::Result<Self> {
        let mut guard = Self {
            terminal,
            restored: false,
        };
        if let Err(error) = guard.suspend_inner() {
            let _ = guard.restore();
            return Err(error);
        }
        Ok(guard)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let mut first_error = None;
        for result in [
            self.terminal.enable_raw_mode(),
            self.terminal.enter_alternate_screen(),
            self.terminal.hide_cursor(),
            self.terminal.redraw(),
        ] {
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn suspend_inner(&mut self) -> io::Result<()> {
        self.terminal.show_cursor()?;
        self.terminal.leave_alternate_screen()?;
        self.terminal.disable_raw_mode()
    }
}

impl Drop for TerminalSuspendGuard<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub fn run_external(
    terminal: &dyn TerminalControl,
    command: &ExternalCommand,
) -> Result<NavigationOutcome, String> {
    install_ctrl_c_handler()?;
    let mut terminal_guard = TerminalSuspendGuard::suspend(terminal)
        .map_err(|error| format!("could not suspend terminal: {error}"))?;
    let _attachment = AttachmentGuard::begin();
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    reset_child_interrupt(&mut process);
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not spawn session attachment: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for session attachment: {error}"))?;
    ATTACHMENT_ACTIVE.store(false, Ordering::SeqCst);
    let cancelled = ATTACHMENT_CANCELLED.swap(false, Ordering::SeqCst);
    let outcome = classify_exit(status.success(), cancelled);
    let restore = terminal_guard
        .restore()
        .map_err(|error| format!("could not restore terminal: {error}"));
    match (outcome, restore) {
        (Ok(NavigationOutcome::Cancelled { .. }), Err(error)) => Ok(NavigationOutcome::Cancelled {
            restore_error: Some(error),
        }),
        (outcome, Ok(())) => outcome,
        (_, Err(error)) => Err(error),
    }
}

pub fn navigate_to_session(
    terminal: &dyn TerminalControl,
    navigation: &dyn SessionNavigation,
    target: &SessionTarget,
) -> Result<NavigationOutcome, String> {
    navigate_to_session_with(navigation, target, |command| {
        run_external(terminal, command)
    })
}

static ATTACHMENT_ACTIVE: AtomicBool = AtomicBool::new(false);
static ATTACHMENT_CANCELLED: AtomicBool = AtomicBool::new(false);
static CTRL_C_HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

struct AttachmentGuard;

impl AttachmentGuard {
    fn begin() -> Self {
        ATTACHMENT_CANCELLED.store(false, Ordering::SeqCst);
        ATTACHMENT_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for AttachmentGuard {
    fn drop(&mut self) {
        ATTACHMENT_ACTIVE.store(false, Ordering::SeqCst);
        ATTACHMENT_CANCELLED.store(false, Ordering::SeqCst);
    }
}

fn install_ctrl_c_handler() -> Result<(), String> {
    CTRL_C_HANDLER
        .get_or_init(|| {
            ctrlc::set_handler(|| {
                if ATTACHMENT_ACTIVE.load(Ordering::SeqCst) {
                    ATTACHMENT_CANCELLED.store(true, Ordering::SeqCst);
                }
            })
            .map_err(|error| format!("could not install Ctrl-C handler: {error}"))
        })
        .clone()
}

#[cfg(unix)]
fn reset_child_interrupt(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs after fork and before exec. `libc::signal` is
    // async-signal-safe and only restores SIGINT's default disposition.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn reset_child_interrupt(_command: &mut Command) {}

fn classify_exit(success: bool, cancelled: bool) -> Result<NavigationOutcome, String> {
    if cancelled {
        Ok(NavigationOutcome::Cancelled {
            restore_error: None,
        })
    } else if success {
        Ok(NavigationOutcome::Attached)
    } else {
        Err("session attachment exited with a nonzero status".into())
    }
}

fn navigate_to_session_with(
    navigation: &dyn SessionNavigation,
    target: &SessionTarget,
    run: impl FnOnce(&ExternalCommand) -> Result<NavigationOutcome, String>,
) -> Result<NavigationOutcome, String> {
    let primary = match navigation.resolve(target) {
        Ok(NavigationPlan::External(command)) => match run(&command) {
            Ok(NavigationOutcome::Cancelled { restore_error }) => {
                return Ok(NavigationOutcome::Cancelled { restore_error });
            }
            Ok(NavigationOutcome::Attached) => return Ok(NavigationOutcome::Attached),
            Ok(NavigationOutcome::FocusedFallback) => {
                return Ok(NavigationOutcome::FocusedFallback);
            }
            Err(error) => error,
        },
        Err(error) => error.to_string(),
    };
    navigation
        .focus_fallback(target)
        .map(|()| NavigationOutcome::FocusedFallback)
        .map_err(|fallback| format!("{primary}; fallback failed: {fallback}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{NavigationError, NavigationPlan};

    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct FakeTerminal {
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeTerminal {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

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

    #[test]
    fn normal_child_exit_restores_terminal_once() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();

        let outcome = run_external(&terminal, &shell_command("exit 0")).unwrap();

        assert_eq!(outcome, NavigationOutcome::Attached);
        assert_eq!(terminal.calls(), expected_lifecycle());
    }

    #[test]
    fn nonzero_child_exit_restores_terminal_once() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();

        let error = run_external(&terminal, &shell_command("exit 7")).unwrap_err();

        assert!(error.contains("status"));
        assert_eq!(terminal.calls(), expected_lifecycle());
    }

    #[test]
    fn spawn_failure_restores_terminal_once() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();
        let command =
            ExternalCommand::new("/definitely/missing/coding-brain-child", [] as [&str; 0]);

        let error = run_external(&terminal, &command).unwrap_err();

        assert!(error.contains("spawn"));
        assert_eq!(terminal.calls(), expected_lifecycle());
    }

    #[test]
    fn explicit_restore_plus_drop_is_idempotent() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();
        {
            let mut guard = TerminalSuspendGuard::suspend(&terminal).unwrap();
            guard.restore().unwrap();
        }

        assert_eq!(terminal.calls(), expected_lifecycle());
    }

    #[test]
    fn panic_path_restores_through_drop() {
        let _lock = test_lock();
        let terminal = Arc::new(FakeTerminal::default());
        let inside = terminal.clone();

        let result = std::panic::catch_unwind(move || {
            let _guard = TerminalSuspendGuard::suspend(inside.as_ref()).unwrap();
            panic!("fixture");
        });

        assert!(result.is_err());
        assert_eq!(terminal.calls(), expected_lifecycle());
    }

    #[test]
    fn handled_cancellation_is_not_an_attach_failure() {
        let _lock = test_lock();
        let outcome = classify_exit(false, true);

        assert_eq!(
            outcome.unwrap(),
            NavigationOutcome::Cancelled {
                restore_error: None
            }
        );
    }

    #[test]
    fn cancellation_never_invokes_fallback() {
        let _lock = test_lock();
        let navigation = FakeNavigation::external();

        let outcome = navigate_to_session_with(&navigation, &target(), |_| {
            Ok(NavigationOutcome::Cancelled {
                restore_error: None,
            })
        })
        .unwrap();

        assert_eq!(
            outcome,
            NavigationOutcome::Cancelled {
                restore_error: None
            }
        );
        assert_eq!(navigation.fallbacks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancellation_with_restore_failure_never_invokes_fallback() {
        let _lock = test_lock();
        let terminal = FailingRestoreTerminal::default();
        let navigation = FakeNavigation::slow_external();
        let cancellation = thread::spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !ATTACHMENT_ACTIVE.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::yield_now();
            }
            ATTACHMENT_CANCELLED.store(true, Ordering::SeqCst);
        });

        let outcome = navigate_to_session(&terminal, &navigation, &target()).unwrap();
        cancellation.join().unwrap();

        assert!(matches!(
            outcome,
            NavigationOutcome::Cancelled {
                restore_error: Some(_)
            }
        ));
        assert_eq!(navigation.fallbacks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ordinary_failure_attempts_fallback_once() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();
        let navigation = FakeNavigation::failed_resolution();

        let outcome = navigate_to_session(&terminal, &navigation, &target()).unwrap();

        assert_eq!(outcome, NavigationOutcome::FocusedFallback);
        assert_eq!(navigation.fallbacks.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_fallback_is_still_attempted_only_once() {
        let _lock = test_lock();
        let terminal = FakeTerminal::default();
        let navigation = FakeNavigation::failed_everywhere();

        let error = navigate_to_session(&terminal, &navigation, &target()).unwrap_err();

        assert!(error.contains("Agent Deck unavailable"));
        assert!(error.contains("fallback failed"));
        assert_eq!(navigation.fallbacks.load(Ordering::SeqCst), 1);
    }

    fn expected_lifecycle() -> Vec<&'static str> {
        vec![
            "show_cursor",
            "leave_alt",
            "raw_off",
            "raw_on",
            "enter_alt",
            "hide_cursor",
            "redraw",
        ]
    }

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn shell_command(script: &str) -> ExternalCommand {
        ExternalCommand::new("/bin/sh", ["-c", script])
    }

    fn classify_exit(success: bool, cancelled: bool) -> Result<NavigationOutcome, String> {
        super::classify_exit(success, cancelled)
    }

    fn target() -> SessionTarget {
        SessionTarget {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            session_id: "session-1".into(),
            turn_id: None,
            tool_use_id: None,
            project_id: ProjectId::Stable("project-1".into()),
            cwd: PathBuf::from("/work/project"),
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        }
    }

    struct FakeNavigation {
        resolution: Result<NavigationPlan, NavigationError>,
        fallback_result: Result<(), String>,
        fallbacks: AtomicUsize,
    }

    impl FakeNavigation {
        fn external() -> Self {
            Self {
                resolution: Ok(NavigationPlan::External(shell_command("exit 1"))),
                fallback_result: Ok(()),
                fallbacks: AtomicUsize::new(0),
            }
        }

        fn failed_resolution() -> Self {
            Self {
                resolution: Err(NavigationError::Unavailable("fixture".into())),
                fallback_result: Ok(()),
                fallbacks: AtomicUsize::new(0),
            }
        }

        fn slow_external() -> Self {
            Self {
                resolution: Ok(NavigationPlan::External(shell_command("sleep 0.1; exit 1"))),
                fallback_result: Ok(()),
                fallbacks: AtomicUsize::new(0),
            }
        }

        fn failed_everywhere() -> Self {
            Self {
                resolution: Err(NavigationError::Unavailable("fixture".into())),
                fallback_result: Err("fixture fallback".into()),
                fallbacks: AtomicUsize::new(0),
            }
        }
    }

    impl SessionNavigation for FakeNavigation {
        fn resolve(&self, _target: &SessionTarget) -> Result<NavigationPlan, NavigationError> {
            self.resolution.clone()
        }

        fn focus_fallback(&self, _target: &SessionTarget) -> Result<(), String> {
            self.fallbacks.fetch_add(1, Ordering::SeqCst);
            self.fallback_result.clone()
        }
    }

    #[derive(Default)]
    struct FailingRestoreTerminal {
        inner: FakeTerminal,
    }

    impl TerminalControl for FailingRestoreTerminal {
        fn show_cursor(&self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn leave_alternate_screen(&self) -> io::Result<()> {
            self.inner.leave_alternate_screen()
        }

        fn disable_raw_mode(&self) -> io::Result<()> {
            self.inner.disable_raw_mode()
        }

        fn enable_raw_mode(&self) -> io::Result<()> {
            self.inner.record("raw_on")?;
            Err(io::Error::other("fixture restore failure"))
        }

        fn enter_alternate_screen(&self) -> io::Result<()> {
            self.inner.enter_alternate_screen()
        }

        fn hide_cursor(&self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn redraw(&self) -> io::Result<()> {
            self.inner.redraw()
        }
    }
}
