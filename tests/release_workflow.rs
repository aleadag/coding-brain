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
    let (_, musl) = workflow.split_once("\n  musl:\n").unwrap();
    for required in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "cargo check --locked --release --target ${{ matrix.target }}",
        "sudo apt-get install -y musl-tools",
        "cargo test --locked --lib --target x86_64-unknown-linux-musl publication_never_replaces_an_existing_final_name",
    ] {
        assert!(
            musl.contains(required),
            "missing musl CI contract: {required}"
        );
    }
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
