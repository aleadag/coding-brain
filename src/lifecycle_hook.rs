#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};

use codexctl_core::lifecycle::{LifecycleEvent, LifecycleStore, compatibility_state_root};

pub(crate) const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookInputError {
    Read,
    TooLarge,
}

impl fmt::Display for HookInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => f.write_str("could not read stdin"),
            Self::TooLarge => f.write_str("input exceeds 65536 bytes"),
        }
    }
}

pub(crate) fn read_bounded_hook_input(mut reader: impl Read) -> Result<Vec<u8>, HookInputError> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|_| HookInputError::Read)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        Err(HookInputError::TooLarge)
    } else {
        Ok(input)
    }
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let _ = writeln!(stderr, "codexctl lifecycle hook: {diagnostic}");
}

pub(crate) fn run_with<R: Read, W: Write, E: Write>(
    stdin: R,
    _stdout: W,
    mut stderr: E,
    store: &LifecycleStore,
) {
    let input = match read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let event = match LifecycleEvent::parse(&input) {
        Ok(event) => event,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    if let Err(error) = store.record(event) {
        write_diagnostic(&mut stderr, error);
    }
}

pub(crate) fn run() {
    let store = LifecycleStore::at(compatibility_state_root());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_with(stdin.lock(), stdout.lock(), stderr.lock(), &store);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use codexctl_core::lifecycle::{LifecycleStore, StoreCondition};

    use super::*;

    const PROMPT: &[u8] = include_bytes!("../tests/fixtures/hooks/user-prompt-submit.json");

    #[test]
    fn bounded_reader_accepts_the_limit_and_rejects_one_more_byte() {
        let exact = vec![b' '; MAX_HOOK_INPUT_BYTES];
        assert_eq!(
            read_bounded_hook_input(Cursor::new(&exact)).unwrap().len(),
            MAX_HOOK_INPUT_BYTES
        );
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(matches!(
            read_bounded_hook_input(Cursor::new(&oversized)),
            Err(HookInputError::TooLarge)
        ));
    }

    #[test]
    fn valid_event_records_state_without_protocol_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(store.read().unwrap().condition, StoreCondition::Healthy);
    }

    #[test]
    fn malformed_or_oversized_input_is_bounded_and_fail_open() {
        for input in [b"secret malformed payload".to_vec(), vec![b'x'; 65_537]] {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with(Cursor::new(input), &mut stdout, &mut stderr, &store);
            assert!(stdout.is_empty());
            let diagnostic = String::from_utf8(stderr).unwrap();
            assert!(diagnostic.starts_with("codexctl lifecycle hook:"));
            assert!(diagnostic.len() < 256);
            assert!(!diagnostic.contains("secret"));
            assert!(!store.snapshot_path().exists());
        }
    }

    #[test]
    fn persistence_failure_and_newer_schema_leave_stdout_empty() {
        let temp = tempfile::tempdir().unwrap();
        let blocked_root = temp.path().join("blocked");
        fs::write(&blocked_root, b"occupied").unwrap();
        let blocked = LifecycleStore::at(&blocked_root);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &blocked);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());

        let store = LifecycleStore::at(temp.path().join("newer"));
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let newer = br#"{"schema_version":2}"#;
        fs::write(store.snapshot_path(), newer).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("newer"));
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), newer);
    }
}
