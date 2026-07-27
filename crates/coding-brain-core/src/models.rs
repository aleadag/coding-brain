pub fn shorten_model(model: &str) -> String {
    let key = model.trim().to_lowercase();
    if key.contains("gpt-5.6-terra") {
        "gpt-5.6-terra".into()
    } else if key.contains("gpt-5.6-luna") {
        "gpt-5.6-luna".into()
    } else if key.contains("gpt-5.6-sol") || key.contains("gpt-5.6") {
        "gpt-5.6-sol".into()
    } else if key.contains("gpt-5.5") {
        "gpt-5.5".into()
    } else if key.contains("gpt-5.4-mini") || key.contains("gpt-5.4 mini") {
        "gpt-5.4-mini".into()
    } else if key.contains("gpt-5.4") {
        "gpt-5.4".into()
    } else {
        model.to_string()
    }
}

pub fn context_window(model: &str) -> Option<u64> {
    match shorten_model(model).as_str() {
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => Some(1_050_000),
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" => Some(258_400),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_is_known_only_for_supported_models() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(context_window(model), Some(1_050_000));
        }
        for model in ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"] {
            assert_eq!(context_window(model), Some(258_400));
        }
        assert_eq!(context_window("custom-model"), None);
    }
}
