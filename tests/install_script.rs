#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    bin: PathBuf,
    install_dir: PathBuf,
    log: PathBuf,
    shell: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let install_dir = root.path().join("install");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        let shell = resolve("sh");
        for command in ["grep", "sed", "mktemp", "rm"] {
            symlink(resolve(command), bin.join(command)).unwrap();
        }
        let fixture = Self {
            log: root.path().join("commands.log"),
            root,
            bin,
            install_dir,
            shell,
        };
        fixture.write_stub(
            "uname",
            r#"case "$1" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) exit 2 ;;
esac"#,
        );
        fixture.write_stub(
            "curl",
            r#"output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output=$2; shift 2 ;;
    *) url=$1; shift ;;
  esac
done
printf 'curl %s\n' "$url" >> "$COMMAND_LOG"
case "$url" in
  */releases/latest) printf '%s\n' '{"tag_name":"v0.58.0"}' ;;
  *.sha256)
    [ "${CHECKSUM_DOWNLOAD_FAIL:-0}" = 0 ] || exit 22
    printf '%s\n' 'unused  coding-brain-v0.58.0-x86_64-unknown-linux-musl.tar.gz' > "$output"
    ;;
  *.tar.gz) : > "$output" ;;
  *) exit 22 ;;
esac"#,
        );
        fixture.write_stub(
            "tar",
            r#"destination=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -C) destination=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n' tar >> "$COMMAND_LOG"
printf '%s\n' binary > "$destination/cbrain""#,
        );
        fixture.write_stub(
            "install",
            r#"{
  printf 'install'
  for argument in "$@"; do printf ' <%s>' "$argument"; done
  printf '\n'
} >> "$COMMAND_LOG"
destination=
for argument in "$@"; do destination=$argument; done
[ -f "$3" ]
: > "$destination""#,
        );
        fixture.write_stub(
            "sudo",
            r#"{
  printf 'sudo'
  for argument in "$@"; do printf ' <%s>' "$argument"; done
  printf '\n'
} >> "$COMMAND_LOG""#,
        );
        fixture
    }

    fn write_stub(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(
            &path,
            format!("#!{}\nset -eu\n{body}\n", self.shell.display()),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn add_verifier(&self, name: &str) {
        self.write_stub(
            name,
            r#"{
  printf '%s' "$0"
  for argument in "$@"; do printf ' <%s>' "$argument"; done
  printf '\n'
} >> "$COMMAND_LOG"
[ "${CHECKSUM_FAIL:-0}" = 0 ]"#,
        );
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.shell);
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .env("PATH", &self.bin)
            .env("INSTALL_DIR", &self.install_dir)
            .env("COMMAND_LOG", &self.log)
            .env_remove("CHECKSUM_DOWNLOAD_FAIL")
            .env_remove("CHECKSUM_FAIL");
        command
    }

    fn run(&self) -> Output {
        self.command().output().unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn resolve(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} is required for this test"))
}

#[test]
fn refuses_to_download_without_a_checksum_verifier() {
    let fixture = Fixture::new();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        fixture.log().is_empty(),
        "downloads occurred: {}",
        fixture.log()
    );
}

#[test]
fn refuses_regular_or_broken_symlink_old_binary_before_download_or_install() {
    for broken_symlink in [false, true] {
        let fixture = Fixture::new();
        fixture.add_verifier("shasum");
        let old = fixture.install_dir.join("coding-brain");
        if broken_symlink {
            symlink(fixture.root.path().join("missing"), &old).unwrap();
        } else {
            fs::write(&old, b"old").unwrap();
        }

        let output = fixture.run();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("remove the existing coding-brain executable")
        );
        assert!(fixture.log().is_empty());
        assert!(fs::symlink_metadata(&old).is_ok());
        assert!(!fixture.install_dir.join("cbrain").exists());
    }
}

#[test]
fn refuses_a_missing_checksum_asset() {
    let fixture = Fixture::new();
    fixture.add_verifier("shasum");
    let output = fixture
        .command()
        .env("CHECKSUM_DOWNLOAD_FAIL", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let log = fixture.log();
    assert!(log.contains(".tar.gz.sha256"));
    assert!(!log.lines().any(|line| line.ends_with(".tar.gz")));
    assert!(!log.contains("\ntar\n"));
    assert!(!log.contains("\ninstall"));
}

#[test]
fn refuses_a_checksum_mismatch() {
    let fixture = Fixture::new();
    fixture.add_verifier("shasum");
    let output = fixture
        .command()
        .env("CHECKSUM_FAIL", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.log().contains("\ntar\n"));
    assert!(!fixture.log().contains("\ninstall"));
}

#[test]
fn verifies_with_shasum_and_installs_with_the_final_mode() {
    let fixture = Fixture::new();
    fixture.add_verifier("shasum");
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fixture.log();
    assert!(log.contains("api.github.com/repos/aleadag/coding-brain/releases/latest"));
    assert!(log.contains("github.com/aleadag/coding-brain/releases/download/v0.58.0/"));
    assert!(log.contains("shasum <-a> <256> <-c> <checksum.sha256>"));
    assert!(log.contains("install <-m> <0755>"));
    assert!(log.lines().any(|line| {
        line.starts_with("install <-m> <0755>")
            && line.contains("/cbrain>")
            && line.ends_with("/install/cbrain>")
    }));
    assert!(!fixture.install_dir.join("coding-brain").exists());
    assert!(fixture.install_dir.join("cbrain").exists());
}

#[test]
fn verifies_with_sha256sum_and_uses_one_privileged_install() {
    let fixture = Fixture::new();
    fixture.add_verifier("sha256sum");
    let missing_destination = fixture.root.path().join("missing");
    let output = fixture
        .command()
        .env("INSTALL_DIR", missing_destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fixture.log();
    assert!(log.contains("sha256sum <-c> <checksum.sha256>"));
    assert!(log.contains("sudo <install> <-m> <0755>"));
    assert!(log.contains("/cbrain>"));
    assert!(log.ends_with("/missing/cbrain>\n"));
    assert!(!log.contains("<mv>"));
    assert!(!log.contains("<chmod>"));
}
