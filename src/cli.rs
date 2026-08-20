//! Noninteractive CLI surface: ask, setup, status, doctor, models, sessions.

use crate::agent::{Agent, Sink};
use crate::config::{Config, KNOWN_MODELS};
use crate::session::{self, Session};
use crate::theme::Theme;
use crate::ui::{self, ShellSink, CANCEL};
use std::io::Write as _;

pub fn ask(config: Config, prompt: &str) -> i32 {
    if config.api_key.is_none() {
        eprintln!("odei: no API key configured; run `odei setup` or set KIMI_API_KEY");
        return 1;
    }
    ui::install_sigint_handler();
    let session = Session::create(&config.workspace_root, &config.model);
    let mut agent = Agent::new(config, session);
    let theme = Theme::detect();
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut sink = ShellSink::new(theme, interactive);
    CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    match agent.run_user_turn(prompt, &CANCEL, &mut sink) {
        Ok(()) => {
            sink.on_text_done();
            0
        }
        Err(e) => {
            eprintln!("odei: {e}");
            1
        }
    }
}

pub fn setup_flow() -> Result<(), String> {
    println!("Set up Kimi Code access.");
    println!("Create an API key in the Kimi Code Console (kimi.com/code), then paste it here.");
    print!("API key: ");
    let _ = std::io::stdout().flush();
    let mut key = String::new();
    std::io::stdin().read_line(&mut key).map_err(|e| e.to_string())?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("no key entered".into());
    }
    let mut stored = crate::config::load_stored();
    stored.api_key = Some(key);
    crate::config::save_stored(&stored).map_err(|e| e.to_string())?;
    println!("Saved to {}", crate::config::odei_home().join("config.json").display());
    Ok(())
}

pub fn status(config: &Config) -> i32 {
    println!("odei {}", env!("CARGO_PKG_VERSION"));
    println!("workspace   {}", config.workspace_root.display());
    println!("model       {}", config.model);
    println!("base url    {}", config.base_url);
    println!("key source  {}", config.key_source);
    println!("permissions {}", config.permission_mode.label());
    println!("cache       {}", if config.prompt_cache { "on" } else { "off" });
    println!("profile     {}", crate::config::odei_home().display());
    0
}

pub fn doctor(config: &Config) -> i32 {
    let mut failures = 0;
    let check = |ok: bool, label: &str, detail: &str| {
        println!("{} {label}{}", if ok { "✓" } else { "✗" }, if detail.is_empty() {
            String::new()
        } else {
            format!(" — {detail}")
        });
        !ok as i32
    };
    failures += check(config.api_key.is_some(), "API key", config.key_source);
    failures += check(
        crate::config::odei_home().exists() || std::fs::create_dir_all(crate::config::odei_home()).is_ok(),
        "profile directory",
        &crate::config::odei_home().display().to_string(),
    );
    failures += check(config.workspace_root.exists(), "workspace", &config.workspace_root.display().to_string());
    if config.api_key.is_some() {
        match crate::provider::check_connectivity(config) {
            Ok(()) => {
                failures += check(true, "kimi connectivity", &config.base_url);
            }
            Err(e) => {
                failures += check(false, "kimi connectivity", &e.to_string());
            }
        }
    }
    if failures == 0 {
        0
    } else {
        1
    }
}

/// Where the published installer lives, pinned to a tag so the script that
/// installs a release is the one that shipped with it.
fn installer_url(tag: &str) -> String {
    format!("https://raw.githubusercontent.com/enekos/odei/{tag}/install.sh")
}

/// The newest published release tag, or None if GitHub can't be reached.
fn latest_release() -> Result<String, String> {
    let body = ureq::get("https://api.github.com/repos/enekos/odei/releases/latest")
        .set("accept", "application/vnd.github+json")
        .set("user-agent", concat!("odei/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    value["tag_name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "the release feed carried no tag_name".to_string())
}

/// `odei upgrade` — check for a newer release and run the published installer.
///
/// The installer is what does the work, so there is one code path for
/// installing and updating, and `upgrade` needs no download, unpack or
/// checksum logic of its own.
pub fn upgrade(check_only: bool) -> i32 {
    let current = env!("CARGO_PKG_VERSION");
    let latest = match latest_release() {
        Ok(tag) => tag,
        Err(e) => {
            eprintln!("odei: could not check for updates: {e}");
            return 1;
        }
    };
    let latest_version = latest.trim_start_matches('v');
    if latest_version == current {
        println!("odei {current} is the latest release.");
        return 0;
    }
    println!("odei {current} is installed; {latest_version} is available.");
    if check_only {
        println!("Run `odei upgrade` to install it.");
        return 0;
    }
    let command = format!("curl -fsSL --proto '=https' --tlsv1.2 {} | sh", installer_url(&latest));
    if which("curl").is_none() || which("sh").is_none() {
        println!("Needs curl and sh. Install it by hand:\n  {command}");
        return 1;
    }
    println!("Running the installer published with {latest}:\n  {command}\n");
    match std::process::Command::new("sh").arg("-c").arg(&command).status() {
        Ok(status) if status.success() => 0,
        Ok(status) => {
            eprintln!("odei: the installer exited with {status}");
            1
        }
        Err(e) => {
            eprintln!("odei: could not run the installer: {e}");
            1
        }
    }
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
}

pub fn models() -> i32 {
    for (id, note) in KNOWN_MODELS {
        println!("{id:<28} {note}");
    }
    0
}

pub fn sessions() -> i32 {
    let sessions = session::list();
    if sessions.is_empty() {
        println!("no saved sessions");
        return 0;
    }
    for s in sessions {
        let title = s.title.unwrap_or_else(|| "(untitled)".into());
        let age = chrono::DateTime::<chrono::Local>::from(s.modified).format("%Y-%m-%d %H:%M");
        println!("{}  {age}  {title}  {} messages  {}", s.id, s.messages, s.workspace);
    }
    0
}

pub fn help() -> i32 {
    println!("odei — tiny native coding agent for the terminal, powered by Kimi");
    println!();
    println!("usage:");
    println!("  odei                       interactive session in the current workspace");
    println!("  odei ask \"<prompt>\"        single noninteractive request");
    println!("  odei sessions              list saved sessions");
    println!("  odei session resume last   resume the latest session for this workspace");
    println!("  odei session resume --id <id>");
    println!("  odei serve [--workspace <dir>] [--resume last|<id>]");
    println!("                             headless NDJSON agent on stdio (drives the macOS app)");
    println!("  odei eval [name…] [--list] run the behavioural evals in ./evals/cases");
    println!("  odei setup                 store a Kimi Code API key");
    println!("  odei upgrade [--check]     update to the latest release");
    println!("  odei status                show runtime configuration");
    println!("  odei doctor                check configuration and connectivity");
    println!("  odei models                list known models");
    println!("  odei version               show the odei version");
    println!();
    println!("environment: KIMI_API_KEY, ODEI_MODEL, ODEI_BASE_URL, ODEI_PERMISSIONS,");
    println!("             ODEI_MAX_AGENT_STEPS, ODEI_PROMPT_CACHE, ODEI_SYSTEM_PROMPT_FILE,");
    println!("             ODEI_EVAL_DIR");
    0
}
