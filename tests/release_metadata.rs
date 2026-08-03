const RELEASE_VERSION: &str = "0.59.0";
const REPOSITORY: &str = "https://github.com/aleadag/coding-brain";
const ROOT_CORE_DEPENDENCY: &str =
    "coding-brain-core = { path = \"crates/coding-brain-core\", version = \"0.59.0\" }";
const ROOT_TUI_DEPENDENCY: &str =
    "coding-brain-tui = { path = \"crates/coding-brain-tui\", version = \"0.59.0\" }";
const TUI_CORE_DEPENDENCY: &str =
    "coding-brain-core = { path = \"../coding-brain-core\", version = \"0.59.0\" }";
const ROOT_PACKAGE_INCLUDE: &str = r#"include = [
    "/src/**",
    "/Cargo.toml",
    "/Cargo.lock",
    "/README.md",
    "/CHANGELOG.md",
    "/LICENSE",
]"#;

fn field<'a>(manifest: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name} = \"");
    let matching_value = |line: &'a str| line.strip_prefix(&prefix)?.strip_suffix('\"');
    manifest.lines().find_map(matching_value).unwrap()
}

#[test]
fn release_packages_are_aligned() {
    let root = include_str!("../Cargo.toml");
    let core = include_str!("../crates/coding-brain-core/Cargo.toml");
    let tui = include_str!("../crates/coding-brain-tui/Cargo.toml");
    assert_eq!(field(root, "version"), RELEASE_VERSION);
    assert_eq!(field(core, "version"), RELEASE_VERSION);
    assert_eq!(field(tui, "version"), RELEASE_VERSION);
    assert!(root.contains(ROOT_CORE_DEPENDENCY));
    assert!(root.contains(ROOT_TUI_DEPENDENCY));
    assert!(tui.contains(TUI_CORE_DEPENDENCY));
}

#[test]
fn published_crates_use_the_current_repository() {
    let core = include_str!("../crates/coding-brain-core/Cargo.toml");
    let tui = include_str!("../crates/coding-brain-tui/Cargo.toml");
    assert_eq!(field(core, "repository"), REPOSITORY);
    assert_eq!(field(tui, "repository"), REPOSITORY);
}

#[test]
fn root_package_uses_exact_publication_allowlist() {
    let root = include_str!("../Cargo.toml");
    assert!(root.contains(ROOT_PACKAGE_INCLUDE));
    assert!(!root.lines().any(|line| line.starts_with("exclude = ")));
}

#[test]
fn changelog_is_cut_for_v0_59_0() {
    let changelog = include_str!("../CHANGELOG.md");
    assert!(changelog.contains("## [Unreleased]\n\n## [0.59.0] - 2026-08-03"));
}

#[test]
fn release_body_covers_the_real_range_and_known_limitations() {
    let notes = include_str!("../.github/releases/v0.59.0.md");
    for required in [
        "v0.57.2...v0.59.0",
        "PreToolUse anchor",
        "BusyBox `time -o`",
        "cargo install coding-brain",
        "brew upgrade coding-brain",
        "https://raw.githubusercontent.com/aleadag/coding-brain/main/install.sh",
    ] {
        assert!(notes.contains(required), "missing {required}");
    }
    assert!(!notes.contains("codexctl-478t"));
    assert!(!notes.contains("scheduler-sensitive"));
}
