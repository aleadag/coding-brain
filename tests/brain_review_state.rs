use serde_json::{Value, json};

fn surface<'a>(observation: &'a Value, name: &str) -> &'a Value {
    &observation["surfaces"][name]
}

fn count(observation: &Value, name: &str, field: &str) -> u64 {
    surface(observation, name)[field].as_u64().unwrap()
}

fn assert_surface_mutation_isolated(before: &Value, after: &Value, changed: &str) {
    for name in ["attention", "review", "diagnostics", "recent"] {
        if name == changed {
            assert_eq!(
                count(after, name, "revision"),
                count(before, name, "revision").checked_add(1).unwrap(),
                "revision for mutated {name} surface"
            );
        } else {
            assert_eq!(
                surface(after, name),
                surface(before, name),
                "non-target {name} surface changed"
            );
        }
    }
}

#[test]
fn surface_mutation_isolation_rejects_non_target_count_drift() {
    let summary = || {
        json!({
            "revision": 0,
            "new": 1,
            "reviewed": 0,
            "last_archive": 0,
            "rows": 1,
        })
    };
    let before = json!({
        "surfaces": {
            "attention": summary(),
            "review": summary(),
            "diagnostics": summary(),
            "recent": summary(),
        }
    });
    let mut after = before.clone();
    after["surfaces"]["attention"]["revision"] = 1.into();
    after["surfaces"]["recent"]["new"] = 2.into();

    let result = std::panic::catch_unwind(|| {
        assert_surface_mutation_isolated(&before, &after, "attention");
    });
    assert!(result.is_err(), "non-target count drift was not rejected");
}
