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
        assert!(
            verify.contains(required),
            "missing verify contract: {required}"
        );
    }
    let build = workflow
        .split_once("\n  build:\n")
        .unwrap()
        .1
        .split_once("\n  publish-core:\n")
        .unwrap()
        .0;
    assert!(build.contains("needs: verify"), "build bypasses verify");
    assert!(
        workflow.contains(
            "tar czf \"../../../coding-brain-${TAG}-${{ matrix.target }}.tar.gz\" cbrain"
        )
    );
    assert!(!workflow.contains(".tar.gz coding-brain\n"));
}

#[test]
fn musl_release_targets_are_checked_before_tagging() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let musl = job(workflow, "musl", "fmt");
    for required in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "cargo check --locked --release --target ${{ matrix.target }}",
        "cargo check --locked --release --features fault-injection --target ${{ matrix.target }}",
        "sudo apt-get install -y musl-tools",
        "- name: Install cross",
        "cargo install cross --locked --git https://github.com/cross-rs/cross --branch main",
        "cross check --locked --release --target ${{ matrix.target }}",
        "cross check --locked --release --features fault-injection --target ${{ matrix.target }}",
        "cargo test --locked --lib --target x86_64-unknown-linux-musl publication_never_replaces_an_existing_final_name",
    ] {
        assert!(
            musl.contains(required),
            "missing musl CI contract: {required}"
        );
    }
}

#[test]
fn ci_bounds_parallel_tests_for_global_process_state() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let test = job(workflow, "test", "core-standalone");
    assert_contract(test, "cargo test --all-targets -- --test-threads=1");
}

#[test]
fn ci_runs_live_fault_matrix_with_isolated_release_artifacts() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let test = job(workflow, "test", "core-standalone");
    for required in [
        "fail-fast: false",
        "CARGO_TARGET_DIR=target/fault-injection-release",
        "cargo test --locked --release --features fault-injection --test live_fault_matrix -- --test-threads=1",
    ] {
        assert_contract(test, required);
    }

    let cache = uses_step(test, "Swatinem/rust-cache@v2");
    assert_contract(cache, "save-if: false");

    let matrix = named_step(test, "Run live fault matrix");
    assert_eq!(
        test.matches("target/fault-injection-release").count(),
        matrix.matches("target/fault-injection-release").count(),
        "the feature target must be referenced only by the live matrix step"
    );
    assert_no_artifact_uploads(test);

    let linkage = named_step(test, "Verify release binary uses bundled SQLite");
    assert_contract(linkage, "CARGO_TARGET_DIR=target/release-default");
    assert!(!linkage.contains("fault-injection"));
}

#[test]
fn ci_requires_nix_package_checks_on_supported_hosts_and_vm_security_on_linux() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let cargo = job(workflow, "test", "nix");
    let nix = job(workflow, "nix", "core-standalone");

    for required in [
        "name: Test (${{ matrix.os }})",
        "runs-on: ${{ matrix.os }}",
        "os: [ubuntu-latest, macos-latest]",
        "cargo test --all-targets -- --test-threads=1",
    ] {
        assert_contract(cargo, required);
    }

    for required in [
        "name: Nix (${{ matrix.os }})",
        "runs-on: ${{ matrix.os }}",
        "os: [ubuntu-latest, macos-latest]",
        "timeout-minutes: 30",
        "cachix/install-nix-action@v31",
        "extra_nix_config: sandbox = true",
        "name: Verify sandbox",
        "nix config show sandbox",
        "name: Evaluate all flake systems",
        "nix flake check --all-systems --no-build",
        "name: Build Nix package",
        "nix build --no-link --print-build-logs .#",
        "name: Test storage security in NixOS VM",
        "nix build --no-link --print-build-logs .#checks.x86_64-linux.storage-security-vm",
    ] {
        assert_contract(nix, required);
    }

    for linux_only_step in [
        "Evaluate all flake systems",
        "Test storage security in NixOS VM",
    ] {
        assert_contract(named_step(nix, linux_only_step), "if: runner.os == 'Linux'");
    }

    for forbidden in [
        "continue-on-error",
        "retry",
        "sudo",
        "bwrap",
        "unshare-user",
    ] {
        assert!(!nix.contains(forbidden), "Nix CI must not use {forbidden}");
    }
}

#[test]
#[should_panic(expected = "test job must not upload artifacts")]
fn artifact_upload_guard_rejects_broad_test_job_uploads() {
    let test_job =
        "\n      - uses: actions/upload-artifact@v4\n        with:\n          path: target/\n";
    assert_no_artifact_uploads(test_job);
}

#[test]
fn official_release_nix_and_package_paths_remain_feature_free() {
    let release = include_str!("../.github/workflows/release.yml");
    let build = job(release, "build", "publish-core");
    assert_contract(build, "cargo build --release --target ${{ matrix.target }}");
    assert_contract(build, "actions/upload-artifact@v4");
    assert_contract(build, "- name: Package");
    assert!(
        !release.contains("fault-injection"),
        "official release build, upload, and publish commands must remain feature-free"
    );

    let flake = include_str!("../flake.nix");
    assert_contract(flake, "buildRustPackage");
    assert!(
        !flake.contains("fault-injection"),
        "Nix build and package commands must remain feature-free"
    );
}

#[test]
fn nix_package_check_is_portable_and_bounded() {
    let flake = include_str!("../flake.nix");

    for forbidden in [
        "bubblewrap",
        "/bin/bwrap",
        "--unshare-user",
        "nixCheckCargo",
        "CBRAIN_NIX_CHECK_UID",
        "CBRAIN_NIX_CHECK_GID",
        "--workspace",
        "--exclude",
    ] {
        assert!(
            !flake.contains(forbidden),
            "Nix package checks must not require {forbidden}"
        );
    }

    for required in [
        "dontUseCargoParallelTests = true;",
        "cargoTestFlags = [",
        "\"coding-brain-core\"",
        "\"coding-brain-tui\"",
        "nativeCheckInputs = [",
        "pkgs.git",
        "pkgs.curl",
        "cargo test",
        "--offline",
        "--test release_workflow",
        "--test release_metadata",
        "--test-threads=1",
    ] {
        assert_contract(flake, required);
    }

    let package = flake
        .split_once("package = pkgs.rustPlatform.buildRustPackage {")
        .expect("Nix must retain the installable Rust package")
        .1
        .split_once("\n        };")
        .expect("Nix Rust package must remain a bounded attribute set")
        .0;
    assert!(
        !package.contains("checkType"),
        "Nix package checks must inherit the release build profile"
    );
    let post_check = package
        .split_once("postCheck = ''")
        .expect("Nix package checks must retain the custom integration post-check")
        .1
        .split_once("'';")
        .expect("Nix package post-check must remain a bounded shell block")
        .0;
    assert_contract(post_check, "cargo test");
    assert_contract(post_check, "--release");

    let darwin = flake
        .split_once("++ pkgs.lib.optionals pkgs.stdenv.isDarwin [")
        .expect("Nix package checks must isolate Darwin-only capability filters")
        .1
        .split_once("];")
        .expect("Darwin-only capability filters must be a bounded list")
        .0;
    for required in [
        "\"--\"",
        "helpers::tests::status_webhook_keeps_only_retained_session_fields",
        "project::tests::git_root_preserves_non_utf8_path_bytes",
    ] {
        assert_contract(darwin, required);
    }
    assert_eq!(
        darwin.matches("\"--skip\"").count(),
        2,
        "Darwin Nix must skip only the two approved capability-dependent tests"
    );
}

#[test]
fn nix_exposes_a_bounded_storage_security_vm_check() {
    let flake = include_str!("../flake.nix");
    let home_manager = include_str!("../nix/tests/home-manager-module.nix");
    let vm = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/nix/tests/storage-security-vm.nix"
    ))
    .expect("storage VM test module must exist");
    let doctor_fixtures = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/nix/tests/home-manager-doctor-fixtures.nix"
    ))
    .expect("Home Manager doctor fixtures must exist");

    for required in [
        "storageSecurityVm = pkgs.testers.runNixOSTest",
        "system == \"x86_64-linux\"",
        "storage-security-vm = storageSecurityVm",
        "requiredFeatures.kvm = false",
        "qemu.forceAccel = false",
        "globalTimeout = 15 * 60",
        "--distill-once",
        "--brain-review list",
        "doctor --json",
        "state directory ancestor is foreign-owned",
        "state directory ancestor is replaceable by another user",
        "home-manager-doctor-fixtures.nix",
        "Home Manager provider hooks pass doctor",
        "invalid Home Manager provider content fails doctor",
    ] {
        assert!(
            flake.contains(required) || vm.contains(required) || doctor_fixtures.contains(required),
            "missing storage VM contract: {required}"
        );
    }
    assert!(!flake.contains("passthru.tests"));
    assert!(
        !home_manager.contains("cbrain doctor --json"),
        "real cbrain storage checks must run in the NixOS VM, not a plain derivation"
    );
}

#[test]
fn ci_release_binary_rejects_dynamic_system_sqlite() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let test = job(workflow, "test", "core-standalone");
    let step = named_step(test, "Verify release binary uses bundled SQLite");
    assert_contract(step, "CARGO_TARGET_DIR=target/release-default");
    assert_contract(step, "cargo build --locked --release");
    assert!(!step.contains("fault-injection"));

    let linux = shell_branch(
        step,
        "if [ \"$RUNNER_OS\" = \"Linux\" ]; then",
        "elif [ \"$RUNNER_OS\" = \"macOS\" ]; then",
    );
    assert_fail_closed_linkage_check(
        linux,
        "linux_linkage",
        "ldd $CARGO_TARGET_DIR/release/cbrain",
        "release binary dynamically requires libsqlite3",
    );

    let macos = shell_branch(step, "elif [ \"$RUNNER_OS\" = \"macOS\" ]; then", "fi");
    assert_fail_closed_linkage_check(
        macos,
        "macos_linkage",
        "otool -L $CARGO_TARGET_DIR/release/cbrain",
        "release binary dynamically requires libsqlite3",
    );
}

fn named_step<'a>(job: &'a str, name: &str) -> &'a str {
    let marker = format!("      - name: {name}\n");
    let (_, step) = job
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing workflow step: {name}"));
    step.split_once("\n      - ").map_or(step, |(step, _)| step)
}

fn uses_step<'a>(job: &'a str, action: &str) -> &'a str {
    let marker = format!("      - uses: {action}\n");
    let (_, step) = job
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing workflow action: {action}"));
    step.split_once("\n      - ").map_or(step, |(step, _)| step)
}

fn assert_no_artifact_uploads(job: &str) {
    assert!(
        !job.contains("actions/upload-artifact"),
        "test job must not upload artifacts"
    );
}

fn shell_branch<'a>(step: &'a str, start: &str, end: &str) -> &'a str {
    let (_, branch) = step
        .split_once(start)
        .unwrap_or_else(|| panic!("missing shell branch: {start}"));
    branch
        .split_once(end)
        .unwrap_or_else(|| panic!("unterminated shell branch: {start}"))
        .0
}

fn assert_fail_closed_linkage_check(
    branch: &str,
    output_variable: &str,
    inspector: &str,
    error: &str,
) {
    let capture = format!("{output_variable}=\"$({inspector})\"");
    let search = format!("grep -F libsqlite3 <<<\"${output_variable}\"");
    let capture_index = branch.find(&capture).unwrap_or_else(|| {
        panic!("{inspector} must be a standalone capture so a nonzero exit fails the step")
    });
    let search_index = branch
        .find(&search)
        .unwrap_or_else(|| panic!("captured {inspector} output is not checked for libsqlite3"));
    assert!(
        capture_index < search_index,
        "{inspector} output must be captured before it is searched"
    );
    assert_contract(branch, error);
    assert_contract(branch, "exit 1");
}

fn job<'a>(workflow: &'a str, name: &str, next: &str) -> &'a str {
    let (_, section) = workflow.split_once(&format!("\n  {name}:\n")).unwrap();
    section.split_once(&format!("\n  {next}:\n")).unwrap().0
}

fn assert_contract(section: &str, required: &str) {
    assert!(section.contains(required), "missing {required}");
}

const CARGO_TOKEN_ENV: &str = "CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}";
const HOMEBREW_TOKEN_ENV: &str = "HOMEBREW_TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}";
const RELEASE_BODY_PATH: &str = "body_path: .github/releases/${{ github.ref_name }}.md";
const OLD_NOTIFY_WORKFLOW: &str = ".github/workflows/issue-release-notify.yml";

#[test]
fn release_preflight_covers_versions_credentials_and_collisions() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let verify = job(workflow, "verify", "build");
    assert_contract(verify, "Cargo.toml");
    assert_contract(verify, "crates/coding-brain-core/Cargo.toml");
    assert_contract(verify, "crates/coding-brain-tui/Cargo.toml");
    assert_contract(verify, CARGO_TOKEN_ENV);
    assert_contract(verify, HOMEBREW_TOKEN_ENV);
    assert_contract(verify, "github.run_attempt == 1");
    assert_contract(verify, "api/v1/crates/${PACKAGE}/${VERSION}");
}

#[test]
fn publication_is_cargo_visible_and_retry_safe() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let core = job(workflow, "publish-core", "publish-tui");
    let tui = job(workflow, "publish-tui", "publish");
    let root = job(workflow, "publish", "release");
    assert_contract(tui, "needs: publish-core");
    assert_contract(root, "needs: publish-tui");
    assert_contract(core, "GITHUB_RUN_ATTEMPT");
    assert_contract(tui, "GITHUB_RUN_ATTEMPT");
    assert_contract(root, "GITHUB_RUN_ATTEMPT");
    assert_contract(core, "already published");
    assert_contract(tui, "already published");
    assert_contract(root, "already published");
    assert!(!core.contains("--token"));
    assert!(!tui.contains("--token"));
    assert!(!root.contains("--token"));
    assert_contract(tui, "cargo info");
    assert_contract(root, "cargo info");
    assert_contract(tui, "seq 1 60");
    assert_contract(root, "seq 1 60");
    assert_contract(tui, "sleep 10");
    assert_contract(root, "sleep 10");
    assert_contract(tui, "cargo publish --dry-run");
    assert_contract(root, "cargo publish --dry-run");
}

#[test]
fn release_permissions_notes_and_binary_versions_are_enforced() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let verify = job(workflow, "verify", "build");
    let release = job(workflow, "release", "update-homebrew");
    let build = job(workflow, "build", "publish-core");
    assert_contract(workflow, "permissions:\n  contents: read");
    assert_contract(release, "permissions:\n      contents: write");
    assert_contract(verify, "NOTES=\".github/releases/${GITHUB_REF_NAME}.md\"");
    assert_contract(verify, "test -s \"$NOTES\"");
    assert_contract(release, RELEASE_BODY_PATH);
    assert_contract(release, "generate_release_notes: true");
    assert_contract(build, "strings \"$BINARY\"");
    assert_contract(build, "\"$BINARY\" --version");
}

#[test]
fn fixed_issue_notification_waits_for_homebrew() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let (_, notify) = workflow.split_once("\n  notify-fixed-issues:\n").unwrap();
    assert_contract(notify, "needs: update-homebrew");
    assert_contract(notify, "contents: read");
    assert_contract(notify, "issues: write");
    assert_contract(notify, "releases/tag/${TAG}");
    assert_contract(notify, "brew upgrade coding-brain");
    assert_contract(notify, "cargo install coding-brain");
    let old = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(OLD_NOTIFY_WORKFLOW);
    assert!(!old.exists());
}
