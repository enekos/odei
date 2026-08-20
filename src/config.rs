//! Profile config under ~/.odei: config.json, sessions/, usage.jsonl.
//! The API key comes from `odei setup`, KIMI_API_KEY, or ODEI_API_KEY.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding";
pub const DEFAULT_MODEL: &str = "kimi-for-coding";
pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("kimi-for-coding", "default coding model, all plans"),
    ("k3", "K3, 1M context (Moderato+)"),
    ("k3-256k", "K3, 256k context (Moderato+)"),
    ("kimi-for-coding-highspeed", "high-speed variant (Allegretto+)"),
];

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionMode>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: Option<String>,
    pub key_source: &'static str,
    pub model: String,
    pub base_url: String,
    pub permission_mode: PermissionMode,
    pub max_agent_steps: usize,
    pub workspace_root: PathBuf,
}

pub fn odei_home() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
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

impl Config {
    pub fn load(workspace_root: &Path) -> Config {
        let stored = load_stored();
        let (api_key, key_source) = if let Ok(k) = std::env::var("KIMI_API_KEY") {
            (Some(k), "KIMI_API_KEY")
        } else if let Ok(k) = std::env::var("ODEI_API_KEY") {
            (Some(k), "ODEI_API_KEY")
        } else if stored.api_key.is_some() {
            (stored.api_key.clone(), "config (odei setup)")
        } else {
            (None, "missing")
        };

        let model = std::env::var("ODEI_MODEL")
            .ok()
            .or_else(|| stored.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let base_url = std::env::var("ODEI_BASE_URL")
            .ok()
            .or_else(|| stored.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();

        let permission_mode = std::env::var("ODEI_PERMISSIONS")
            .ok()
            .and_then(|v| PermissionMode::parse(&v))
            .or(stored.permissions)
            .unwrap_or(PermissionMode::Auto);

        let max_agent_steps = std::env::var("ODEI_MAX_AGENT_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        Config {
            api_key,
            key_source,
            model,
            base_url,
            permission_mode,
            max_agent_steps,
            workspace_root: workspace_root.to_path_buf(),
        }
    }

    pub fn context_window(&self) -> u64 {
        match self.model.as_str() {
            "k3" => 1_048_576,
            "k3-256k" | "kimi-for-coding" | "kimi-for-coding-highspeed" => 262_144,
            _ => 262_144,
        }
    }
}
