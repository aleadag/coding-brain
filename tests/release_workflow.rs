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
}
