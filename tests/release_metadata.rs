const RELEASE_VERSION: &str = "0.59.0";
const REPOSITORY: &str = "https://github.com/aleadag/coding-brain";
const ROOT_CORE_DEPENDENCY: &str =
    "coding-brain-core = { path = \"crates/coding-brain-core\", version = \"0.59.0\" }";
const ROOT_TUI_DEPENDENCY: &str =
    "coding-brain-tui = { path = \"crates/coding-brain-tui\", version = \"0.59.0\" }";
const TUI_CORE_DEPENDENCY: &str =
    "coding-brain-core = { path = \"../coding-brain-core\", version = \"0.59.0\" }";

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
