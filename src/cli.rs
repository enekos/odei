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
