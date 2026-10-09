use crate::config::{Config, Provider};

pub fn canonical(name: &str) -> String {
    match name
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
        .as_str()
    {
        "deepseek" => "deepseek".into(),
        "gemini" => "gemini".into(),
        "googleaimode" => "google-ai-mode".into(),
        _ => name.to_owned(),
    }
}

pub fn catalog() -> Vec<Provider> {
    #[derive(serde::Deserialize)]
    struct Catalog {
        providers: Vec<Provider>,
    }
    let mut result = Config::defaults().providers;
    result.extend(
        toml::from_str::<Catalog>(include_str!("../assets/providers.catalog.toml"))
            .expect("valid provider catalog")
            .providers,
    );
    result
}

pub fn select_default(config: &mut Config, name: &str) -> Result<(), String> {
    if name.eq_ignore_ascii_case("all") {
        if config.providers.is_empty() {
            return Err("no providers are configured".into());
        }
        config.default_all = true;
        return Ok(());
    }
    let id = canonical(name);
    if config.provider(&id).is_none() {
        return Err(format!("provider {name} is not configured"));
    }
    config.default_provider = Some(id);
    config.default_all = false;
    Ok(())
}

pub fn drop_provider(config: &mut Config, name: &str) -> Result<String, String> {
    let id = canonical(name);
    if config.provider(&id).is_none() {
        return Err(format!("provider {name} is not configured"));
    }
    config.providers.retain(|p| p.id != id);
    if config.providers.is_empty() {
        config.default_all = false;
    }
    if config.default_provider.as_deref() == Some(&id) {
        config.default_provider = config.providers.first().map(|p| p.id.clone());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropping_the_default_uses_the_first_remaining_provider_and_preserves_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[browser]\nheadless = false\n").unwrap();
        let mut config = Config::defaults();
        config.providers = catalog();
        select_default(&mut config, "Gemini").unwrap();
        assert_eq!(config.default_model_id(), Some("gemini"));
        drop_provider(&mut config, "Gemini").unwrap();
        assert_eq!(config.default_model_id(), Some("deepseek-chat"));
        crate::config::save_providers(&path, &config).unwrap();
        let loaded = Config::load(Some(&path)).unwrap();
        assert!(!loaded.browser.headless);
        assert!(loaded.provider("gemini").is_none());
        drop_provider(&mut config, "deepseek").unwrap();
        select_default(&mut config, "all").unwrap();
        drop_provider(&mut config, "GoogleAIMode").unwrap();
        assert!(!config.default_all);
        crate::config::save_providers(&path, &config).unwrap();
        assert!(Config::load(Some(&path)).unwrap().providers.is_empty());
    }
}
