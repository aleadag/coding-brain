#[cfg(not(feature = "fault-injection"))]
use std::fs;
#[cfg(not(feature = "fault-injection"))]
use std::os::unix::fs::PermissionsExt;
#[cfg(not(feature = "fault-injection"))]
use std::path::Path;
#[cfg(not(feature = "fault-injection"))]
use std::process::{Command, Stdio};

#[cfg(not(feature = "fault-injection"))]
use coding_brain::brain::storage::{MigrationCoordinator, MigrationStatus};

#[cfg(not(feature = "fault-injection"))]
#[test]
fn default_binary_rejects_every_fault_argument() {
    for arguments in [
        &["--fault-point", "admission-write"][..],
        &["--migration-fault-stage", "building"][..],
        &["--fault-capability", "/tmp/cbrain-capability"][..],
        &["--fault-nonce", "nonce"][..],
        &["--fault-control-fd", "3"][..],
        &["--fault-worker"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cbrain"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
            "wrong rejection for {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(not(feature = "fault-injection"))]
#[test]
fn default_binary_ignores_legacy_migration_fault_environment() {
    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
                fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
    }

    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let state = temp.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let state_root = state.join("coding-brain");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/storage/permission-journal-4vh58"),
        &state_root,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .arg("--distill-once")
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", &state)
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1")
        .env(
            "CODING_BRAIN_SQLITE_MIGRATION_FAULT",
            "after-brain-publication",
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        MigrationCoordinator::at(&state_root).inspect().unwrap(),
        MigrationStatus::Complete
    );
}

#[cfg(feature = "fault-injection")]
mod feature {
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tempfile::TempDir;

    use coding_brain::brain::storage::{BrainDb, StoragePaths};

    struct ActivationFixture {
        _temp: TempDir,
        state_base: PathBuf,
        state_root: PathBuf,
        capability: PathBuf,
        nonce: String,
        _control_reader: File,
        control_writer: File,
    }

    impl ActivationFixture {
        fn matrix(point: &str) -> Self {
            Self::new(json!({ "kind": "matrix", "selection": point }))
        }

        fn migration(stage: &str) -> Self {
            Self::new(json!({
                "kind": "migration-regression",
                "selection": stage,
            }))
        }

        fn new(selection: serde_json::Value) -> Self {
            let temp = tempfile::tempdir().unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let state_base = temp.path().join("state");
            let state_root = state_base.join("coding-brain");
            fs::create_dir_all(&state_root).unwrap();
            fs::set_permissions(&state_base, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();

            let fifo = temp.path().join("control.fifo");
            let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
            let control_reader = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
                .unwrap();
            let control_writer = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
                .unwrap();
            let descriptor_flags =
                unsafe { libc::fcntl(control_writer.as_raw_fd(), libc::F_GETFD) };
            assert!(descriptor_flags >= 0);
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        control_writer.as_raw_fd(),
                        libc::F_SETFD,
                        descriptor_flags & !libc::FD_CLOEXEC,
                    )
                },
                0
            );

            let metadata = control_writer.metadata().unwrap();
            let nonce = "test-nonce-2o9fo".to_owned();
            let capability = temp.path().join("capability.json");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&capability)
                .unwrap();
            serde_json::to_writer(
                &mut file,
                &json!({
                    "version": 1,
                    "state_root": state_root,
                    "nonce": nonce,
                    "selection": selection,
                    "control_device": metadata.dev(),
                    "control_inode": metadata.ino(),
                }),
            )
            .unwrap();
            file.flush().unwrap();

            Self {
                _temp: temp,
                state_base,
                state_root,
                capability,
                nonce,
                _control_reader: control_reader,
                control_writer,
            }
        }

        fn run(&self, role: &str, selector: [&str; 2]) -> Output {
            self.command(role, selector).output().unwrap()
        }

        fn read_marker(&mut self) -> Vec<u8> {
            let mut marker = [0_u8; 512];
            let length = self._control_reader.read(&mut marker).unwrap();
            marker[..length].to_vec()
        }

        fn fill_control_pipe(&mut self) {
            let bytes = [b'x'; 4096];
            loop {
                match self.control_writer.write(&bytes) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("could not fill fault control pipe: {error}"),
                }
            }
        }

        fn command(&self, role: &str, selector: [&str; 2]) -> Command {
            let mut command = self.base_command(role, selector);
            command
                .arg("--fault-capability")
                .arg(&self.capability)
                .arg("--fault-nonce")
                .arg(&self.nonce)
                .arg("--fault-control-fd")
                .arg(self.control_writer.as_raw_fd().to_string());
            command
        }

        fn base_command(&self, role: &str, selector: [&str; 2]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
            command
                .env("XDG_STATE_HOME", &self.state_base)
                .env("HOME", self._temp.path())
                .args([role, selector[0], selector[1]])
                .stdin(Stdio::null());
            command
        }
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn run_bounded(mut command: Command) -> Output {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().unwrap() {
                Some(_) => return child.wait_with_output().unwrap(),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "cbrain timed out\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn assert_rejected_before_storage(output: Output, fixture: &ActivationFixture, message: &str) {
        assert!(!output.status.success(), "unexpected success");
        assert!(
            !fixture.state_root.join("db").exists(),
            "validation opened SQLite storage"
        );
        assert!(stderr(&output).contains(message), "{}", stderr(&output));
    }

    #[test]
    fn feature_binary_hides_fault_arguments() {
        let help = Command::new(env!("CARGO_BIN_EXE_cbrain"))
            .arg("--help")
            .output()
            .unwrap();
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        for argument in [
            "fault-point",
            "migration-fault-stage",
            "fault-capability",
            "fault-nonce",
            "fault-control-fd",
            "fault-worker",
        ] {
            assert!(!help.contains(argument), "help revealed --{argument}");
        }
    }

    #[test]
    fn feature_binary_requires_the_complete_activation_tuple() {
        let output = Command::new(env!("CARGO_BIN_EXE_cbrain"))
            .args(["--permission-hook", "--fault-point", "admission-write"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("complete fault activation is required"),
            "{}",
            stderr(&output)
        );
    }

    #[test]
    fn feature_binary_rejects_both_selectors_before_storage() {
        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture
            .command("--permission-hook", ["--fault-point", "admission-write"])
            .args(["--migration-fault-stage", "building"])
            .output()
            .unwrap();
        assert_rejected_before_storage(output, &fixture, "exactly one fault selector is required");
    }

    #[test]
    fn feature_binary_rejects_invalid_descriptor_and_capability_before_storage() {
        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture
            .base_command("--permission-hook", ["--fault-point", "admission-write"])
            .arg("--fault-capability")
            .arg(&fixture.capability)
            .arg("--fault-nonce")
            .arg(&fixture.nonce)
            .args(["--fault-control-fd", "2"])
            .output()
            .unwrap();
        assert_rejected_before_storage(output, &fixture, "fault control descriptor is reserved");

        let fixture = ActivationFixture::matrix("admission-write");
        let missing = fixture._temp.path().join("missing-capability");
        let mut command =
            fixture.base_command("--permission-hook", ["--fault-point", "admission-write"]);
        let output = command
            .arg("--fault-capability")
            .arg(&missing)
            .arg("--fault-nonce")
            .arg(&fixture.nonce)
            .arg("--fault-control-fd")
            .arg(fixture.control_writer.as_raw_fd().to_string())
            .output()
            .unwrap();
        assert_rejected_before_storage(output, &fixture, "MigrationRequired");
    }

    #[test]
    fn feature_binary_rejects_selector_capability_mismatch_before_storage() {
        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture.run("--permission-hook", ["--fault-point", "inference-exit"]);
        assert_rejected_before_storage(
            output,
            &fixture,
            "fault capability selection does not match",
        );
    }

    #[test]
    fn feature_binary_accepts_a_valid_hook_activation() {
        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture.run("--permission-hook", ["--fault-point", "admission-write"]);
        assert!(output.status.success(), "{}", stderr(&output));
    }

    #[test]
    fn feature_binary_enforces_hook_and_worker_selection_ownership() {
        for point in [
            "admission-write",
            "inference-exit",
            "commit-before-call",
            "commit-after-return",
            "stdout-write",
            "delivery-write",
        ] {
            let fixture = ActivationFixture::matrix(point);
            let output = fixture.run("--fault-worker", ["--fault-point", point]);
            assert_rejected_before_storage(
                output,
                &fixture,
                "fault point requires permission-hook role",
            );
        }

        for point in ["checkpoint", "migration-publish"] {
            let fixture = ActivationFixture::matrix(point);
            let output = fixture.run("--permission-hook", ["--fault-point", point]);
            assert_rejected_before_storage(
                output,
                &fixture,
                "fault point requires fault-worker role",
            );
        }

        let fixture = ActivationFixture::migration("building");
        let output = fixture.run("--permission-hook", ["--migration-fault-stage", "building"]);
        assert_rejected_before_storage(output, &fixture, "fault point requires fault-worker role");
    }

    #[test]
    fn migration_worker_rejects_an_unconsumed_selection() {
        let fixture = ActivationFixture::matrix("migration-publish");
        drop(
            BrainDb::create_current(&StoragePaths::at(&fixture.state_root))
                .unwrap_or_else(|error| panic!("{}: {error:?}", fixture.state_root.display())),
        );
        let output = fixture.run("--fault-worker", ["--fault-point", "migration-publish"]);
        assert!(!output.status.success(), "unexpected success");
        assert!(
            stderr(&output).contains("fault selection was not consumed"),
            "{}",
            stderr(&output)
        );
    }

    #[test]
    fn checkpoint_worker_dispatches_through_non_hook_storage() {
        let mut fixture = ActivationFixture::matrix("checkpoint");
        drop(
            BrainDb::create_current(&StoragePaths::at(&fixture.state_root))
                .unwrap_or_else(|error| panic!("{}: {error:?}", fixture.state_root.display())),
        );
        let output = fixture.run("--fault-worker", ["--fault-point", "checkpoint"]);
        assert!(
            !output.status.success(),
            "checkpoint fault unexpectedly succeeded"
        );
        assert_eq!(
            fixture.read_marker(),
            b"CBRAIN-FAULT-V1\0checkpoint\0before\0-\n"
        );
    }

    #[test]
    fn worker_prioritizes_marker_failure_over_checkpoint_failure() {
        let mut fixture = ActivationFixture::matrix("checkpoint");
        drop(
            BrainDb::create_current(&StoragePaths::at(&fixture.state_root))
                .unwrap_or_else(|error| panic!("{}: {error:?}", fixture.state_root.display())),
        );
        fixture.fill_control_pipe();
        let output = fixture.run("--fault-worker", ["--fault-point", "checkpoint"]);
        assert!(!output.status.success(), "unexpected success");
        assert!(
            stderr(&output).contains("fault marker emission failed"),
            "{}",
            stderr(&output)
        );
    }

    #[test]
    fn feature_binary_requires_exactly_one_fault_role() {
        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture
            .command("--permission-hook", ["--fault-point", "admission-write"])
            .arg("--fault-worker")
            .output()
            .unwrap();
        assert_rejected_before_storage(output, &fixture, "exactly one fault role is required");

        let fixture = ActivationFixture::matrix("admission-write");
        let output = fixture
            .command("--headless", ["--fault-point", "admission-write"])
            .output()
            .unwrap();
        assert_rejected_before_storage(output, &fixture, "exactly one fault role is required");
    }

    #[test]
    fn permission_hook_activation_rejects_higher_precedence_dispatch_modes() {
        for competing_arguments in [
            &["--shell-safety-helper"][..],
            &["--lifecycle-hook"][..],
            &["--recovery-hook"][..],
            &["storage", "reset-review-state"][..],
        ] {
            let fixture = ActivationFixture::matrix("admission-write");
            let output = fixture
                .command("--permission-hook", ["--fault-point", "admission-write"])
                .args(competing_arguments)
                .output()
                .unwrap();
            assert_rejected_before_storage(
                output,
                &fixture,
                "fault activation cannot be combined with a higher-precedence mode",
            );
        }
    }

    #[test]
    fn ambient_environment_does_not_enable_fault_arguments() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for directory in [
            temp.path().join("config"),
            temp.path().join("state"),
            temp.path().join("state/coding-brain"),
            temp.path().join("state/coding-brain/brain"),
        ] {
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
        command
            .args(["config", "validate"])
            .current_dir(temp.path())
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env("CODING_BRAIN_SKIP_FIRST_RUN", "1")
            .env("CBRAIN_FAULT_POINT", "admission-write")
            .env("CBRAIN_FAULT_NONCE", "ambient")
            .env("CBRAIN_FAULT_CONTROL_FD", "3");
        let output = run_bounded(command);
        assert!(output.status.success(), "{}", stderr(&output));
    }

    #[test]
    fn hook_json_cannot_supply_fault_activation() {
        let temp = tempfile::tempdir().unwrap();
        let hook_json = temp.path().join("hook.json");
        fs::write(
            &hook_json,
            r#"{"fault_point":"admission-write","fault_nonce":"ambient"}"#,
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cbrain"))
            .args(["--permission-hook"])
            .stdin(File::open(hook_json).unwrap())
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }
}
