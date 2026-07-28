# Release Gating and Installer Verification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Prevent tag releases from publishing without fresh quality checks and make the binary installer refuse every unverified archive.

**Architecture:** Keep the existing release dependency graph and strengthen its single `verify` entry gate. Exercise `install.sh` as a black box with a hermetic command path, then make checksum verification and final-mode installation fail closed.

**Tech Stack:** GitHub Actions YAML, POSIX shell, Rust integration tests, `tempfile`

## Global Constraints

- Keep all existing publishing jobs transitively dependent on `build → verify`.
- Run exactly `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` in the release verification job.
- Support both `shasum -a 256 -c` and `sha256sum -c`.
- Never extract or install when the checksum asset, verifier, or verification result is unavailable.
- Use `install -m 0755` directly or through `sudo`; do not retain a `mv` plus `chmod` path.
- Use only the canonical `aleadag/coding-brain` repository in installer URLs.
- Do not refactor normal CI, change release artifacts, or modify unrelated packaging.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Fail-Closed Binary Installer

**Files:**
- Create: `tests/install_script.rs`
- Modify: `install.sh:3`
- Modify: `install.sh:7`
- Modify: `install.sh:43`

**Interfaces:**
- Consumes: the published archive naming contract `coding-brain-<tag>-<target>.tar.gz`
- Produces: a black-box installer contract enforced by `cargo test --test install_script`

**Acceptance Criteria:**
- Installer requests release metadata and assets only from `aleadag/coding-brain`.
- Installer exits before archive download when neither supported checksum utility is available.
- Missing checksum downloads and failed checksum validation prevent extraction and installation.
- Both checksum utility command forms are covered.
- Writable destinations use `install -m 0755`; non-writable or absent destinations use `sudo install -m 0755`.

- [ ] **Step 1: Create the hermetic installer fixture**

Create `tests/install_script.rs` with Unix-only helpers that resolve the test
shell and required ordinary commands from the current `PATH`, build a temporary
stub-only `PATH`, and invoke the repository script:

```rust
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
printf '%s\n' binary > "$destination/coding-brain""#,
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
        fs::write(&path, format!("#!{}\nset -eu\n{body}\n", self.shell.display())).unwrap();
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
```

- [ ] **Step 2: Write installer failure and success tests**

Append black-box cases to `tests/install_script.rs`:

```rust
#[test]
fn refuses_to_download_without_a_checksum_verifier() {
    let fixture = Fixture::new();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(fixture.log().is_empty(), "downloads occurred: {}", fixture.log());
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
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let log = fixture.log();
    assert!(log.contains("api.github.com/repos/aleadag/coding-brain/releases/latest"));
    assert!(log.contains("github.com/aleadag/coding-brain/releases/download/v0.58.0/"));
    assert!(log.contains("shasum <-a> <256> <-c> <checksum.sha256>"));
    assert!(log.contains("install <-m> <0755>"));
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
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let log = fixture.log();
    assert!(log.contains("sha256sum <-c> <checksum.sha256>"));
    assert!(log.contains("sudo <install> <-m> <0755>"));
    assert!(!log.contains("<mv>"));
    assert!(!log.contains("<chmod>"));
}
```

- [ ] **Step 3: Run the focused test to prove the current installer fails**

Run:

```bash
cargo test --test install_script
```

Expected: failures for the stale repository URL, missing-verifier fail-open
path, missing-checksum fail-open path, and `mv`/`chmod` installation path.
Stop and investigate before implementation if the failures do not map to those
expected contract gaps.

- [ ] **Step 4: Implement the minimal fail-closed installer**

Update `install.sh` to select a checker before any network access, require both
downloads, verify from the temporary directory, and install with the final
mode:

```sh
# Usage: curl -fsSL https://raw.githubusercontent.com/aleadag/coding-brain/main/install.sh | sh

REPO="aleadag/coding-brain"

if command -v shasum >/dev/null 2>&1; then
    verify_checksum() {
        shasum -a 256 -c "$1"
    }
elif command -v sha256sum >/dev/null 2>&1; then
    verify_checksum() {
        sha256sum -c "$1"
    }
else
    echo "Error: checksum verification requires shasum or sha256sum" >&2
    exit 1
fi

curl -fsSL -o "${TMP_DIR}/checksum.sha256" "$CHECKSUM_URL"
curl -fsSL -o "${TMP_DIR}/${ARCHIVE}" "$URL"

(
    cd "$TMP_DIR"
    verify_checksum checksum.sha256
)

tar xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"

if [ -w "$INSTALL_DIR" ]; then
    install -m 0755 "${TMP_DIR}/coding-brain" "${INSTALL_DIR}/coding-brain"
else
    echo "Installing to ${INSTALL_DIR} (requires sudo)..."
    sudo install -m 0755 "${TMP_DIR}/coding-brain" "${INSTALL_DIR}/coding-brain"
fi
```

Do not retain the conditional checksum download, verifier fallthrough,
`mv`, or trailing `chmod`.

- [ ] **Step 5: Run focused installer verification**

Run:

```bash
cargo test --test install_script
cargo fmt --all -- --check
shellcheck install.sh
```

Expected: all installer tests pass, formatting reports no changes, and
ShellCheck reports no findings.

### Task 2: Mandatory Tag Verification Gate

**Files:**
- Create: `tests/release_workflow.rs`
- Modify: `.github/workflows/release.yml:15`

**Interfaces:**
- Consumes: the existing `verify → build → publish → release` job dependency graph
- Produces: a tag workflow whose `verify` job runs the release-critical quality suite

**Acceptance Criteria:**
- The tag verification job installs Rustfmt and Clippy.
- It runs the exact formatting, Clippy, and all-target test commands from the approved spec.
- The build matrix remains directly dependent on `verify`.
- Every publishing and packaging job remains transitively downstream of the build matrix.

- [ ] **Step 1: Write a failing workflow contract test**

Create `tests/release_workflow.rs`:

```rust
#[test]
fn tag_release_runs_the_release_critical_quality_suite_before_building() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let verify = workflow
        .split_once("  verify:\n")
        .unwrap()
        .1
        .split_once("\n  build:\n")
        .unwrap()
        .0;
    for required in [
        "components: rustfmt, clippy",
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets -- -D warnings",
        "cargo test --all-targets",
    ] {
        assert!(verify.contains(required), "missing verify contract: {required}");
    }
    let build = workflow
        .split_once("\n  build:\n")
        .unwrap()
        .1
        .split_once("\n  publish-core:\n")
        .unwrap()
        .0;
    assert!(build.contains("needs: verify"), "build bypasses verify");
}
```

- [ ] **Step 2: Run the workflow contract test to prove the gate is absent**

Run:

```bash
cargo test --test release_workflow
```

Expected: FAIL reporting the missing Rust components and quality commands.

- [ ] **Step 3: Extend the existing verification job**

In `.github/workflows/release.yml`, keep the tag/version step and add the
toolchain components, cache, and named quality steps:

```yaml
  verify:
    name: Verify tag
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: Ensure Cargo version matches tag
        run: |
          TAG="${GITHUB_REF#refs/tags/v}"
          VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
          test "$TAG" = "$VERSION"
      - name: Check formatting
        run: cargo fmt --all -- --check
      - name: Run Clippy
        run: cargo clippy --all-targets -- -D warnings
      - name: Run tests
        run: cargo test --all-targets
```

Do not change `build.needs: verify` or any downstream `needs` edge.

- [ ] **Step 4: Run focused and full verification**

Run:

```bash
cargo test --test release_workflow
cargo test --test install_script
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
shellcheck install.sh
git diff --check
git status --short
```

Expected: all tests and checks pass; status lists only the approved spec, plan,
installer, release workflow, and two regression-test files.

- [ ] **Step 5: Prepare the handoff without publishing**

Report the changed files, exact command results, `codexctl-sevs` status, and the
suggested emoji conventional commit description. Do not commit, push, or publish
without new authorization.

## Stress Test Results: Release Hardening Implementation Plan

### Resolved Decisions

- Bound workflow assertions to the actual `verify` and `build` job blocks so
  commands in unrelated jobs cannot satisfy the contract test.
- Preserve a stub-only Unix command path with the test shell and ordinary
  utilities resolved from the test process environment.
- Inspect the initial red test output and stop if failures do not match the
  known installer contract gaps.
- Fetch the mandatory checksum asset before downloading the archive.
- Keep function-based checksum dispatch and exercise both supported utilities.
- Run ShellCheck in focused and final verification.
- Preserve the no-commit, no-push, and no-release handoff boundary.
- Remove test-control environment variables by default and set them only in
  their intended failure cases.

### Changes Made

- Strengthened the workflow contract test to check job placement.
- Strengthened the missing-checksum test and reversed asset download order.
- Added `shellcheck install.sh` to verification.
- Centralized installer command construction so ambient test-control variables
  cannot contaminate hermetic cases.

### Deferred / Parking Lot

- `actionlint` is unavailable locally; live GitHub Actions execution remains the
  external workflow-syntax verification surface.

### Confidence Assessment

- Overall: High
- Areas of concern: the GitHub Actions workflow cannot be executed locally, so
  its final runtime proof requires a future tag run.
