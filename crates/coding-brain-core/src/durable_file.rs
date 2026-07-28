use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path};

/// Atomically replaces `path` after durably syncing a same-directory temp file.
///
/// `path` must have an already-established parent hierarchy, and `temp_prefix`
/// must be one non-empty normal path component. The trusted writer callback
/// receives a private temp file; Unix mode `0600` is enforced before and after
/// the callback.
///
/// On Unix, success means both file contents and the destination-directory
/// entry completed their durability barriers. On non-Unix platforms, file
/// contents are synced and replacement is atomic, while directory-entry crash
/// durability is best-effort.
///
/// # Errors
///
/// Before replacement, an error preserves the old destination. A
/// directory-sync error occurs after replacement, so the complete new file may
/// already be visible even though crash durability is uncertain. Filesystem
/// errors are converted through `E: From<io::Error>`; callback errors retain
/// their caller-defined type.
pub fn durable_replace<E, F>(path: &Path, temp_prefix: &str, write: F) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
{
    durable_replace_with(path, temp_prefix, write, File::sync_all, sync_parent)
}

fn durable_replace_with<E, F, S, D>(
    path: &Path,
    temp_prefix: &str,
    write: F,
    sync_file: S,
    sync_directory: D,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
    S: FnOnce(&File) -> io::Result<()>,
    D: FnOnce(&Path) -> io::Result<()>,
{
    let temp_prefix = std::ffi::OsStr::new(temp_prefix);
    let mut prefix_components = Path::new(temp_prefix).components();
    if !matches!(
        (prefix_components.next(), prefix_components.next()),
        (Some(Component::Normal(component)), None) if component == temp_prefix
    ) {
        return Err(E::from(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable replacement temp prefix must be one normal path component",
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            E::from(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable replacement path has no parent",
            ))
        })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(temp_prefix)
        .tempfile_in(parent)
        .map_err(E::from)?;
    set_file_mode(temporary.as_file()).map_err(E::from)?;
    write(temporary.as_file_mut())?;
    set_file_mode(temporary.as_file()).map_err(E::from)?;
    temporary.flush().map_err(E::from)?;
    sync_file(temporary.as_file()).map_err(E::from)?;
    temporary
        .persist(path)
        .map_err(|error| E::from(error.error))?;
    sync_directory(parent).map_err(E::from)
}

#[cfg(unix)]
fn set_file_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;

    fn matching_temps(parent: &Path, prefix: &str) -> Vec<std::path::PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn successful_replacement_writes_complete_contents_without_temp_leak() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        durable_replace::<io::Error, _>(&path, "state.tmp-", |file| file.write_all(b"new"))
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn writer_failure_preserves_old_contents_without_temp_leak() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let error = durable_replace::<io::Error, _>(&path, "state.tmp-", |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("writer failed"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn invalid_temp_prefixes_are_rejected_before_file_creation() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        fs::create_dir(&private).unwrap();
        let nested = private.join("nested");
        fs::create_dir(&nested).unwrap();
        let path = private.join("state.json");
        fs::write(&path, b"old").unwrap();

        for prefix in ["", "/absolute-", "../escaped-", "nested/", "./nested"] {
            let error =
                durable_replace::<io::Error, _>(&path, prefix, |file| file.write_all(b"new"))
                    .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(fs::read_dir(&nested).unwrap().next().is_none());
        let entries = fs::read_dir(root.path())
            .unwrap()
            .map(Result::unwrap)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [std::ffi::OsString::from("private")]);
    }

    #[test]
    fn file_sync_failure_preserves_old_contents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let result = durable_replace_with::<io::Error, _, _, _>(
            &path,
            "state.tmp-",
            |file| file.write_all(b"new"),
            |_| Err(io::Error::other("file sync failed")),
            |_| Ok(()),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn directory_sync_failure_reports_error_after_complete_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let result = durable_replace_with::<io::Error, _, _, _>(
            &path,
            "state.tmp-",
            |file| file.write_all(b"new"),
            |_| Ok(()),
            |_| Err(io::Error::other("directory sync failed")),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn raw_readers_observe_only_complete_replacements() {
        let root = tempfile::tempdir().unwrap();
        let path = Arc::new(root.path().join("state.json"));
        let first = vec![b'a'; 64 * 1024];
        let second = vec![b'b'; 64 * 1024];
        fs::write(path.as_ref(), &first).unwrap();

        let reader_path = Arc::clone(&path);
        let reader_first = first.clone();
        let reader_second = second.clone();
        let (reader_ready_tx, reader_ready_rx) = mpsc::channel();
        let (reader_stop_tx, reader_stop_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut first_read = true;
            loop {
                let observed = fs::read(reader_path.as_ref()).unwrap();
                assert!(observed == reader_first || observed == reader_second);
                if first_read {
                    reader_ready_tx.send(()).unwrap();
                    first_read = false;
                }
                match reader_stop_rx.try_recv() {
                    Ok(()) => break,
                    Err(mpsc::TryRecvError::Empty) => std::thread::yield_now(),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("reader stop channel disconnected")
                    }
                }
            }
        });

        reader_ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("reader exceeded test-only startup deadline");
        let writer_path = Arc::clone(&path);
        let writer_first = first.clone();
        let writer_second = second.clone();
        let (writer_done_tx, writer_done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            for index in 0..8 {
                let payload = if index % 2 == 0 {
                    &writer_second
                } else {
                    &writer_first
                };
                durable_replace::<io::Error, _>(writer_path.as_ref(), "state.tmp-", |file| {
                    file.write_all(payload)
                })
                .unwrap();
            }
            writer_done_tx.send(()).unwrap();
        });

        writer_done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("writer exceeded test-only completion deadline");
        reader_stop_tx.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_reasserts_private_permissions_after_writer() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        durable_replace::<io::Error, _>(&path, "state.tmp-", |file| {
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file.write_all(b"new")
        })
        .unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
