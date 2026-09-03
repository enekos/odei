//! Profile config under ~/.odei: config.json, sessions/, usage.jsonl.
//! The API key comes from `odei setup`, KIMI_API_KEY / GEMINI_API_KEY, or
//! ODEI_API_KEY.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding";
pub const DEFAULT_MODEL: &str = "kimi-for-coding";
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-2.5-flash";
pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("kimi-for-coding", "default coding model, all plans"),
    ("k3", "K3, 1M context (Moderato+)"),
    ("k3-256k", "K3, 256k context (Moderato+)"),
    (
        "kimi-for-coding-highspeed",
        "high-speed variant (Allegretto+)",
    ),
    ("gemini-2.5-flash", "Gemini 2.5 Flash (GEMINI_API_KEY)"),
    (
        "gemini-flash-latest",
        "newest Gemini Flash (GEMINI_API_KEY)",
    ),
    ("gemini-pro-latest", "newest Gemini Pro (GEMINI_API_KEY)"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Kimi,
    Gemini,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Kimi => "kimi",
            Provider::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "kimi" => Some(Provider::Kimi),
            "gemini" | "google" => Some(Provider::Gemini),
            _ => None,
        }
    }

    /// The model id names the provider for every model odei knows about;
    /// ODEI_PROVIDER exists for base-url overrides pointing at proxies whose
    /// model ids follow neither family.
    pub fn infer(model: &str) -> Option<Self> {
        if model.starts_with("gemini") {
            Some(Provider::Gemini)
        } else if model.starts_with("kimi") || model.starts_with("k3") {
            Some(Provider::Kimi)
        } else {
            None
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Kimi => DEFAULT_MODEL,
            Provider::Gemini => GEMINI_DEFAULT_MODEL,
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Kimi => DEFAULT_BASE_URL,
            Provider::Gemini => GEMINI_BASE_URL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Ask,
    Auto,
    Yolo,
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::Auto => "auto",
            PermissionMode::Yolo => "YOLO",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ask" => Some(PermissionMode::Ask),
            "auto" => Some(PermissionMode::Auto),
            "yolo" => Some(PermissionMode::Yolo),
            _ => None,
        }
    }
}

/// How much of each tool call the shell shows. `Normal` is the default: one
/// line per call, plus the diff when a file changed — the two things you
/// almost always want. `/collapse` and `/expand` move between the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    /// One line per call, nothing under it.
    Collapsed,
    /// One line per call, plus diffs and the reason a call failed.
    Normal,
    /// Everything: arguments, full diffs, output, per-step token accounting,
    /// and the model's thinking as it streams.
    Expanded,
}

impl Detail {
    pub fn label(self) -> &'static str {
        match self {
            Detail::Collapsed => "collapsed",
            Detail::Normal => "normal",
            Detail::Expanded => "expanded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "collapsed" | "collapse" | "compact" | "off" | "min" => Some(Detail::Collapsed),
            "normal" | "default" | "auto" => Some(Detail::Normal),
            // Not "all": /expand all reprints history, it does not set a level.
            "expanded" | "expand" | "full" | "on" | "verbose" => Some(Detail::Expanded),
            _ => None,
        }
    }

    /// Lines of diff shown under a call. Enough at `Normal` to read a
    /// hand-sized edit without scrolling anything off the screen.
    pub fn diff_lines(self) -> usize {
        match self {
            Detail::Collapsed => 0,
            Detail::Normal => 16,
            Detail::Expanded => 200,
        }
    }

    /// Lines of tool output shown under a call that succeeded.
    pub fn output_lines(self) -> usize {
        match self {
            Detail::Collapsed | Detail::Normal => 0,
            Detail::Expanded => 20,
        }
    }

    /// Lines of output shown under a call that failed. A failure is the one
    /// thing worth reading even when everything else is folded away.
    pub fn error_lines(self) -> usize {
        match self {
            Detail::Collapsed => 0,
            Detail::Normal => 6,
            Detail::Expanded => 20,
        }
    }

    pub fn shows_steps(self) -> bool {
        matches!(self, Detail::Expanded)
    }

    pub fn shows_thinking(self) -> bool {
        matches!(self, Detail::Expanded)
    }

    pub fn shows_arguments(self) -> bool {
        matches!(self, Detail::Expanded)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Detail>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: Option<String>,
    pub key_source: &'static str,
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    pub permission_mode: PermissionMode,
    /// How much of each tool call the shell draws (ODEI_DETAIL, /expand).
    pub detail: Detail,
    pub max_agent_steps: usize,
    pub workspace_root: PathBuf,
    /// Mark cache breakpoints on the tools, the system prompt and the tail of
    /// the transcript. On by default; the provider turns it off for the
    /// process if the endpoint rejects it.
    pub prompt_cache: bool,
    /// Override for the system prompt, for A/B-ing prompt changes under
    /// `odei eval` (ODEI_SYSTEM_PROMPT_FILE).
    pub system_prompt_file: Option<PathBuf>,
}

pub fn odei_home() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".odei")
}

pub fn sessions_dir() -> PathBuf {
    odei_home().join("sessions")
}

fn config_path() -> PathBuf {
    odei_home().join("config.json")
}

pub fn load_stored() -> StoredConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => StoredConfig::default(),
    }
}

pub fn save_stored(stored: &StoredConfig) -> std::io::Result<()> {
    let dir = odei_home();
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(stored).expect("config serializes");
    let path = config_path();
    std::fs::write(&path, text)?;
    // The config holds the API key; keep it owner-readable only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn resolve_provider(model: Option<&str>, stored_model: Option<&str>) -> Provider {
    std::env::var("ODEI_PROVIDER")
        .ok()
        .and_then(|v| Provider::parse(&v))
        .or_else(|| model.and_then(Provider::infer))
        .or_else(|| stored_model.and_then(Provider::infer))
        .unwrap_or(Provider::Kimi)
}

fn resolve_key(provider: Provider, stored: &StoredConfig) -> (Option<String>, &'static str) {
    match provider {
        Provider::Kimi => {
            if let Ok(k) = std::env::var("KIMI_API_KEY") {
                (Some(k), "KIMI_API_KEY")
            } else if let Ok(k) = std::env::var("ODEI_API_KEY") {
                (Some(k), "ODEI_API_KEY")
            } else if stored.api_key.is_some() {
                (stored.api_key.clone(), "config (odei setup)")
            } else {
                (None, "missing")
            }
        }
        Provider::Gemini => {
            if let Ok(k) = std::env::var("GEMINI_API_KEY") {
                (Some(k), "GEMINI_API_KEY")
            } else if let Ok(k) = std::env::var("ODEI_API_KEY") {
                (Some(k), "ODEI_API_KEY")
            } else if stored.gemini_api_key.is_some() {
                (stored.gemini_api_key.clone(), "config (odei setup gemini)")
            } else {
                (None, "missing")
            }
        }
    }
}

fn resolve_base_url(provider: Provider, stored: &StoredConfig) -> String {
    std::env::var("ODEI_BASE_URL")
        .ok()
        .or_else(|| stored.base_url.clone())
        .unwrap_or_else(|| provider.default_base_url().to_string())
        .trim_end_matches('/')
        .to_string()
}

impl Config {
    pub fn load(workspace_root: &Path) -> Config {
        let stored = load_stored();
        let env_model = std::env::var("ODEI_MODEL").ok();
        let provider = resolve_provider(env_model.as_deref(), stored.model.as_deref());
        let model = env_model
            .or_else(|| stored.model.clone())
            .unwrap_or_else(|| provider.default_model().to_string());
        let (api_key, key_source) = resolve_key(provider, &stored);
        let base_url = resolve_base_url(provider, &stored);

        let permission_mode = std::env::var("ODEI_PERMISSIONS")
            .ok()
            .and_then(|v| PermissionMode::parse(&v))
            .or(stored.permissions)
            .unwrap_or(PermissionMode::Auto);

        let detail = std::env::var("ODEI_DETAIL")
            .ok()
            .and_then(|v| Detail::parse(&v))
            .or(stored.detail)
            .unwrap_or(Detail::Normal);

        let max_agent_steps = std::env::var("ODEI_MAX_AGENT_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        let prompt_cache = std::env::var("ODEI_PROMPT_CACHE")
            .ok()
            .map(|v| !matches!(v.trim(), "0" | "off" | "false" | "no"))
            .or(stored.prompt_cache)
            .unwrap_or(true);

        let system_prompt_file = std::env::var_os("ODEI_SYSTEM_PROMPT_FILE")
            .map(PathBuf::from)
            .filter(|p| p.exists());

        Config {
            api_key,
            key_source,
            provider,
            model,
            base_url,
            permission_mode,
            detail,
            max_agent_steps,
            workspace_root: workspace_root.to_path_buf(),
            prompt_cache,
            system_prompt_file,
        }
    }

    /// Switching the model can switch the provider, and with it the key and
    /// the base url — `/model gemini-2.5-flash` mid-session must not keep
    /// talking to Kimi.
    pub fn apply_model(&mut self, model: &str) {
        self.model = model.to_string();
        let stored = load_stored();
        let provider = resolve_provider(Some(model), None);
        if provider != self.provider {
            self.provider = provider;
            let (api_key, key_source) = resolve_key(provider, &stored);
            self.api_key = api_key;
            self.key_source = key_source;
            self.base_url = resolve_base_url(provider, &stored);
        }
    }

    /// Output ceiling for one turn. Kept well under the window so a long
    /// transcript plus a long answer still fits.
    pub fn max_tokens(&self) -> u64 {
        match self.provider {
            Provider::Gemini => 65_536,
            Provider::Kimi => match self.model.as_str() {
                "k3" | "k3-256k" => 65_536,
                _ => 32_768,
            },
        }
    }

    pub fn context_window(&self) -> u64 {
        match (self.provider, self.model.as_str()) {
            (Provider::Gemini, _) => 1_048_576,
            (Provider::Kimi, "k3") => 1_048_576,
            _ => 262_144,
        }
    }
}
