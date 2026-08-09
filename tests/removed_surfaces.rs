use std::fs;
use std::path::{Path, PathBuf};

use coding_brain_core::runtime::{BrainRuntime, MockBrainRuntime};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Item, Lit, LitStr, Meta, Token, Type};

const LEGACY_STORAGE_NAMES: [&str; 7] = [
    "activity.jsonl",
    "canonical.jsonl",
    "decisions.jsonl",
    "preferences.json",
    "review-state.json",
    "lifecycle.json",
    "permission-transactions",
];

const LEGACY_STORE_TYPES: [&str; 5] = [
    "ActivityStore",
    "LifecycleStore",
    "PermissionTransactionStore",
    "RecoveryReservationStore",
    "ReviewStateStore",
];

const LEGACY_WRITER_FUNCTIONS: [&str; 4] = [
    "ensure_hook_record_at",
    "ensure_hook_record_at_bounded",
    "mark_canonical",
    "read_canonical_ids",
];

const LEGACY_COMPATIBILITY_MODULES: [&str; 3] = [
    "src/brain/storage/legacy.rs",
    "src/brain/storage/migration.rs",
    "src/brain/storage/export.rs",
];

#[test]
fn final_runtime_exposes_only_brain_source_actions_and_navigation() {
    let runtime: BrainRuntime = MockBrainRuntime::default().into_runtime();

    assert!(runtime.source.refresh(Default::default()).is_ok());
    assert_eq!(runtime.source.gate_mode().as_str(), "on");
}

#[test]
fn removed_dashboard_and_management_flags_are_rejected() {
    let binary = env!("CARGO_BIN_EXE_cbrain");
    for flag in [
        "--list",
        "--watch",
        "--new",
        "--resume",
        "--budget",
        "--record",
        "--clean",
        "--history",
        "--terminal-auto-approve-fallback",
    ] {
        let output = std::process::Command::new(binary)
            .arg(flag)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flag} unexpectedly succeeded");
    }
}

#[test]
fn production_legacy_storage_names_are_confined_to_compatibility_modules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = rust_sources_under(&root.join("src"));
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let entry = entry.unwrap();
        sources.extend(rust_sources_under(&entry.path().join("src")));
    }

    let mut violations = Vec::new();
    for path in sources {
        let relative = path.strip_prefix(root).unwrap();
        let relative_text = relative.to_string_lossy();
        if LEGACY_COMPATIBILITY_MODULES.contains(&relative_text.as_ref())
            || relative_text.starts_with("src/brain/storage/legacy/")
        {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        for violation in scan_source(&source).unwrap() {
            violations.push(format!("{}: {violation}", relative.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "legacy storage escaped migration/export boundaries:\n{}",
        violations.join("\n")
    );
}

#[test]
fn test_only_item_filter_does_not_hide_later_production_code() {
    assert_scan(
        r#"#[cfg(test)] fn fixture() { let _ = "activity.jsonl"; }
            fn production() { let _ = "decisions.jsonl"; }"#,
        &["decisions.jsonl"],
        &["activity.jsonl"],
    );
}

#[test]
fn test_only_item_filter_ignores_cfg_text_in_comments() {
    assert_scan(
        r#"/* #[cfg(test)] documents a test-only alternative. */
            fn production() { let _ = ActivityStore::at("current.sqlite3"); }"#,
        &["ActivityStore::at"],
        &[],
    );
}

#[test]
fn test_only_item_filter_ignores_braces_in_strings_and_comments() {
    assert_scan(
        r#"#[cfg(test)] fn fixture() { let _ = "{"; /* unmatched { */ }
            fn production() { let _ = "preferences.json"; }"#,
        &["preferences.json"],
        &["activity.jsonl"],
    );
}

#[test]
fn explicit_legacy_store_test_support_is_confined_like_cfg_test() {
    assert_scan(
        r#"#[cfg(any(test, feature = "legacy-store-test-support"))]
            mod test_support { struct LifecycleStore; impl LifecycleStore { fn at() {} } }
            fn production() { let _ = "decisions.jsonl"; }"#,
        &["decisions.jsonl"],
        &["LifecycleStore"],
    );
}

#[test]
fn boundary_patterns_cover_definitions_direct_writers_and_learning_sidecars() {
    for name in ["canonical.jsonl", "preferences.json"] {
        assert!(
            LEGACY_STORAGE_NAMES.contains(&name),
            "missing legacy storage name {name}"
        );
    }
    assert!(LEGACY_STORE_TYPES.contains(&"ActivityStore"));
    for function in [
        "ensure_hook_record_at",
        "ensure_hook_record_at_bounded",
        "mark_canonical",
        "read_canonical_ids",
    ] {
        assert!(
            LEGACY_WRITER_FUNCTIONS.contains(&function),
            "missing legacy writer function {function}"
        );
    }
}

#[test]
fn line_wrapped_legacy_constructor_and_definition_are_detected() {
    for store in LEGACY_STORE_TYPES {
        let source = format!(
            "pub struct\n {store}; impl {store} {{}}\n\
             fn production() {{ let _ = {store}::\n at(\"current.sqlite3\"); }}"
        );
        let violations = scan_source(&source).unwrap();

        assert!(has_violation(&violations, &format!("struct {store}")));
        assert!(has_violation(&violations, &format!("impl {store}")));
        assert!(has_violation(&violations, &format!("{store}::at")));
    }
}

#[test]
fn aliased_legacy_constructor_is_detected() {
    assert_scan(
        r#"fn production() { let constructor = ActivityStore::at;
            let _ = constructor("current.sqlite3"); }"#,
        &["ActivityStore::at"],
        &[],
    );
}

fn scan_source(source: &str) -> syn::Result<Vec<String>> {
    let file = syn::parse_file(source)?;
    if file.attrs.iter().any(is_test_harness_cfg) {
        return Ok(Vec::new());
    }
    let mut visitor = LegacySurfaceVisitor::default();
    visitor.visit_file(&file);
    Ok(visitor.violations)
}

fn has_violation(violations: &[String], surface: &str) -> bool {
    violations
        .iter()
        .any(|violation| violation.contains(surface))
}

fn assert_scan(source: &str, present: &[&str], absent: &[&str]) {
    let violations = scan_source(source).unwrap();
    for surface in present {
        assert!(has_violation(&violations, surface), "missed {surface}");
    }
    for surface in absent {
        assert!(!has_violation(&violations, surface), "found {surface}");
    }
}

#[derive(Default)]
struct LegacySurfaceVisitor {
    violations: Vec<String>,
}

impl LegacySurfaceVisitor {
    fn record(&mut self, violation: impl Into<String>) {
        self.violations.push(violation.into());
    }
}

impl<'ast> Visit<'ast> for LegacySurfaceVisitor {
    fn visit_item(&mut self, item: &'ast Item) {
        if item_attributes(item).iter().any(is_test_harness_cfg) {
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_lit_str(&mut self, literal: &'ast LitStr) {
        let value = literal.value();
        for name in LEGACY_STORAGE_NAMES {
            if value.contains(name) {
                self.record(format!("references {name}"));
            }
        }
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        let mut segments = path.path.segments.iter().rev();
        if segments.next().is_some_and(|segment| segment.ident == "at")
            && let Some(store) = segments.next().map(|segment| segment.ident.to_string())
            && LEGACY_STORE_TYPES.contains(&store.as_str())
        {
            self.record(format!("references {store}::at"));
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        let store = item.ident.to_string();
        if LEGACY_STORE_TYPES.contains(&store.as_str()) {
            self.violations.push(format!("defines struct {store}"));
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let store = match item.self_ty.as_ref() {
            Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string()),
            _ => None,
        };
        if store
            .as_deref()
            .is_some_and(|store| LEGACY_STORE_TYPES.contains(&store))
        {
            self.violations
                .push(format!("defines impl {}", store.unwrap()));
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.check_function(&item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if item.attrs.iter().any(is_test_harness_cfg) {
            return;
        }
        visit::visit_impl_item_fn(self, item);
    }

    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        if !attribute.path().is_ident("doc") {
            visit::visit_attribute(self, attribute);
        }
    }
}

impl LegacySurfaceVisitor {
    fn check_function(&mut self, function: &str) {
        if LEGACY_WRITER_FUNCTIONS.contains(&function) {
            self.record(format!("defines function {function}"));
        }
    }
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn is_test_harness_cfg(attribute: &Attribute) -> bool {
    if !attribute.path().is_ident("cfg") {
        return false;
    }
    let Meta::List(cfg) = &attribute.meta else {
        return false;
    };
    let Some(predicates) = meta_arguments(cfg) else {
        return false;
    };
    let [predicate] = predicates.as_slice() else {
        return false;
    };
    if matches!(predicate, Meta::Path(path) if path.is_ident("test")) {
        return true;
    }
    let Meta::List(any) = predicate else {
        return false;
    };
    if !any.path.is_ident("any") {
        return false;
    }
    let Some(arguments) = meta_arguments(any) else {
        return false;
    };
    let [Meta::Path(test), feature] = arguments.as_slice() else {
        return false;
    };
    matches!(
        feature,
        Meta::NameValue(value)
            if test.is_ident("test") && value.path.is_ident("feature")
                && matches!(
                    &value.value,
                    Expr::Lit(literal)
                        if matches!(
                            &literal.lit,
                            Lit::Str(value) if value.value() == "legacy-store-test-support"
                        )
                )
    )
}

fn meta_arguments(list: &syn::MetaList) -> Option<Vec<Meta>> {
    list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok()
        .map(IntoIterator::into_iter)
        .map(Iterator::collect)
}

fn rust_sources_under(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    if !root.is_dir() {
        return sources;
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources.extend(rust_sources_under(&path));
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
    sources
}
