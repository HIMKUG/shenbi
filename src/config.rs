use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_OUTPUT_DIR: &str = "~/Downloads/generated-images";

/// Providers that can turn a prompt into an image.
pub const GENERATE_PROVIDERS: [&str; 6] = ["openai", "gemini", "replicate", "together", "grok", "ideogram"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ProviderConfig {
    pub api_key: String,
    pub base_url: String,
    /// Model used for text-to-image generation.
    pub model: String,
    /// Model used for image editing (openai only).
    pub edit_model: String,
}

impl ProviderConfig {
    fn new(model: &str) -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: model.to_string(),
            edit_model: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub default_provider: String,
    pub output_dir: String,
    pub input_root: String,
    pub run_retention_hours: u32,
    pub max_concurrent: usize,
    pub request_timeout_ms: u64,
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// Providers whose key arrived from the environment; their keys are never
    /// written back to disk.
    #[serde(skip)]
    pub env_keys: std::collections::BTreeSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("openai".into(), ProviderConfig::new("gpt-image-1"));
        providers.insert("gemini".into(), ProviderConfig::new("gemini-3.1-flash-image"));
        providers.insert("replicate".into(), ProviderConfig::new("black-forest-labs/flux-1.1-pro"));
        providers.insert("together".into(), ProviderConfig::new("black-forest-labs/FLUX.1-schnell"));
        providers.insert("grok".into(), ProviderConfig::new("grok-imagine-image-2.0"));
        providers.insert("ideogram".into(), ProviderConfig::new("ideogram-v3-0"));
        providers.insert("photoroom".into(), ProviderConfig::new(""));
        providers.insert("fal".into(), ProviderConfig::new("fal-ai/flux-pro/kontext"));
        providers.insert("anthropic".into(), ProviderConfig::new("claude-haiku-4-5"));
        if let Some(openai) = providers.get_mut("openai") {
            openai.edit_model = "gpt-image-1.5".into();
        }

        Self {
            default_provider: "openai".into(),
            output_dir: DEFAULT_OUTPUT_DIR.into(),
            input_root: String::new(),
            run_retention_hours: 24,
            max_concurrent: 3,
            request_timeout_ms: 300_000,
            providers,
            path: None,
            env_keys: Default::default(),
        }
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Some(custom) = env_value("SHENBI_CONFIG") {
            return PathBuf::from(custom);
        }
        crate::util::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".shenbi")
            .join("config.json")
    }

    pub fn load(path: PathBuf) -> Self {
        let mut config = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Config>(&raw).unwrap_or_else(|err| {
                eprintln!("config: ignoring unreadable {}: {err}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        config.path = Some(path);
        config.apply_env_overrides();
        config.fill_missing_providers();
        config
    }

    /// Environment variables win over the stored file, so CI and shell setups
    /// keep working without writing keys to disk.
    fn apply_env_overrides(&mut self) {
        if let Some(v) = env_value("SHENBI_DEFAULT_PROVIDER") {
            self.default_provider = v;
        }
        if let Some(v) = env_value("SHENBI_OUTPUT_DIR") {
            self.output_dir = v;
        }
        if let Some(v) = env_value("SHENBI_INPUT_ROOT") {
            self.input_root = v;
        }
        let keys = [
            ("openai", "OPENAI_API_KEY"),
            ("gemini", "GEMINI_API_KEY"),
            ("replicate", "REPLICATE_API_TOKEN"),
            ("together", "TOGETHER_API_KEY"),
            ("grok", "XAI_API_KEY"),
            ("ideogram", "IDEOGRAM_API_KEY"),
            ("photoroom", "PHOTOROOM_API_KEY"),
            ("fal", "FAL_KEY"),
            ("anthropic", "ANTHROPIC_API_KEY"),
        ];
        for (provider, var) in keys {
            if let Some(v) = env_value(var) {
                self.ensure_provider(provider).api_key = v;
                self.env_keys.insert(provider.to_string());
            }
        }
        if let Some(v) = env_value("OPENAI_DEFAULT_MODEL") {
            self.ensure_provider("openai").model = v;
        }
        if let Some(v) = env_value("OPENAI_EDIT_MODEL") {
            self.ensure_provider("openai").edit_model = v;
        }
    }

    /// Keep every known provider present so the UI has a stable row set.
    fn fill_missing_providers(&mut self) {
        for (name, entry) in Config::default().providers {
            let slot = self.providers.entry(name).or_insert_with(|| entry.clone());
            if slot.model.is_empty() {
                slot.model = entry.model;
            }
        }
    }

    fn ensure_provider(&mut self, name: &str) -> &mut ProviderConfig {
        self.providers.entry(name.to_string()).or_default()
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    pub fn is_configured(&self, name: &str) -> bool {
        self.provider(name)
            .map(|p| !p.api_key.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn available_generate_providers(&self) -> Vec<&'static str> {
        GENERATE_PROVIDERS
            .into_iter()
            .filter(|name| self.is_configured(name))
            .collect()
    }

    pub fn output_dir_resolved(&self) -> PathBuf {
        crate::util::expand_tilde(if self.output_dir.trim().is_empty() {
            DEFAULT_OUTPUT_DIR
        } else {
            &self.output_dir
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = self.path.clone().unwrap_or_else(Config::default_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut clone = self.clone();
        clone.path = None;
        // Never persist secrets that came from the environment.
        for name in &self.env_keys {
            if let Some(entry) = clone.providers.get_mut(name) {
                entry.api_key.clear();
            }
        }
        std::fs::write(&path, serde_json::to_string_pretty(&clone)?)?;
        Ok(())
    }
}
