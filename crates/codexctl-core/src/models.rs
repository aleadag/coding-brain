use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelProfile {
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_per_m: f64,
    pub context_max: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOverride {
    pub name: String,
    pub profile: ModelProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProfileSource {
    BuiltIn,
    Override,
    Fallback,
}

impl ModelProfileSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Override => "override",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelProfile {
    pub key: String,
    pub profile: ModelProfile,
    pub source: ModelProfileSource,
}

static MODEL_OVERRIDES: OnceLock<Mutex<HashMap<String, ModelProfile>>> = OnceLock::new();

pub fn shorten_model(model: &str) -> String {
    let key = model.trim().to_lowercase();
    if key.contains("gpt-5.5") {
        "gpt-5.5".into()
    } else if key.contains("gpt-5.4-mini") || key.contains("gpt-5.4 mini") {
        "gpt-5.4-mini".into()
    } else if key.contains("gpt-5.4") {
        "gpt-5.4".into()
    } else {
        model.to_string()
    }
}

pub fn set_overrides(overrides: Vec<ModelOverride>) {
    let store = MODEL_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = store.lock() else {
        return;
    };
    guard.clear();
    for override_ in overrides {
        let raw = override_.name.trim().to_lowercase();
        let shortened = shorten_model(&override_.name).to_lowercase();
        guard.insert(raw, override_.profile);
        guard.insert(shortened, override_.profile);
    }
}

pub fn resolve(model: &str) -> ResolvedModelProfile {
    let empty = HashMap::new();
    let store = MODEL_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = store.lock().ok();
    let overrides = guard.as_deref().unwrap_or(&empty);
    resolve_with_overrides(model, overrides)
}

pub(crate) fn resolve_with_overrides(
    model: &str,
    overrides: &HashMap<String, ModelProfile>,
) -> ResolvedModelProfile {
    let raw_key = model.trim().to_lowercase();
    let short_key = shorten_model(model).to_lowercase();

    if let Some(profile) = overrides
        .get(&raw_key)
        .or_else(|| overrides.get(&short_key))
        .copied()
    {
        return ResolvedModelProfile {
            key: if raw_key.is_empty() {
                short_key
            } else {
                raw_key
            },
            profile,
            source: ModelProfileSource::Override,
        };
    }

    if let Some(profile) = built_in_profile(&short_key) {
        return ResolvedModelProfile {
            key: short_key,
            profile,
            source: ModelProfileSource::BuiltIn,
        };
    }

    ResolvedModelProfile {
        key: if short_key.is_empty() {
            "unknown".into()
        } else {
            short_key
        },
        profile: fallback_profile(),
        source: ModelProfileSource::Fallback,
    }
}

fn built_in_profile(key: &str) -> Option<ModelProfile> {
    match key {
        "gpt-5.5" => Some(ModelProfile {
            input_per_m: 5.0,
            output_per_m: 30.0,
            cache_read_per_m: 0.5,
            cache_write_per_m: 5.0,
            context_max: 258_400,
        }),
        "gpt-5.4" => Some(ModelProfile {
            input_per_m: 2.5,
            output_per_m: 15.0,
            cache_read_per_m: 0.25,
            cache_write_per_m: 2.5,
            context_max: 258_400,
        }),
        "gpt-5.4-mini" => Some(ModelProfile {
            input_per_m: 0.75,
            output_per_m: 4.5,
            cache_read_per_m: 0.075,
            cache_write_per_m: 0.75,
            context_max: 258_400,
        }),
        _ => None,
    }
}

fn fallback_profile() -> ModelProfile {
    ModelProfile {
        input_per_m: 5.0,
        output_per_m: 30.0,
        cache_read_per_m: 0.5,
        cache_write_per_m: 5.0,
        context_max: 258_400,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_builtin_profile() {
        let resolved = resolve_with_overrides("gpt-5.5", &HashMap::new());
        assert_eq!(resolved.source, ModelProfileSource::BuiltIn);
        assert_eq!(resolved.profile.input_per_m, 5.0);
        assert_eq!(resolved.profile.cache_read_per_m, 0.5);
        assert_eq!(resolved.profile.output_per_m, 30.0);
        assert_eq!(resolved.profile.context_max, 258_400);
    }

    #[test]
    fn resolve_gpt_family_profiles() {
        let large = resolve_with_overrides("gpt-5.4", &HashMap::new());
        assert_eq!(large.source, ModelProfileSource::BuiltIn);
        assert_eq!(large.profile.input_per_m, 2.5);
        assert_eq!(large.profile.cache_read_per_m, 0.25);
        assert_eq!(large.profile.output_per_m, 15.0);

        let mini = resolve_with_overrides("gpt-5.4-mini", &HashMap::new());
        assert_eq!(mini.source, ModelProfileSource::BuiltIn);
        assert_eq!(mini.profile.input_per_m, 0.75);
        assert_eq!(mini.profile.cache_read_per_m, 0.075);
        assert_eq!(mini.profile.output_per_m, 4.5);
    }

    #[test]
    fn resolve_override_profile() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".into(),
            ModelProfile {
                input_per_m: 1.0,
                output_per_m: 2.0,
                cache_read_per_m: 0.5,
                cache_write_per_m: 1.5,
                context_max: 128_000,
            },
        );
        let resolved = resolve_with_overrides("custom-model", &overrides);
        assert_eq!(resolved.source, ModelProfileSource::Override);
        assert_eq!(resolved.profile.context_max, 128_000);
    }

    #[test]
    fn resolve_fallback_profile() {
        let resolved = resolve_with_overrides("mystery-model", &HashMap::new());
        assert_eq!(resolved.source, ModelProfileSource::Fallback);
        assert_eq!(resolved.profile.context_max, 258_400);
    }
}
