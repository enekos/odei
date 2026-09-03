//! Noninteractive CLI surface: ask, setup, status, doctor, models, sessions.
//!
//! `ask` is also the machine-readable surface. `--output-format json` answers
//! with one object — the reply, the session, what ran, what it cost — and
//! `stream-json` writes the same NDJSON events `odei serve` speaks, so a
//! script can watch a turn instead of waiting for it. Anything piped in on
//! stdin joins the prompt, which makes `odei ask` an ordinary filter.

use crate::agent::{Agent, Approval, Sink, ToolDone, ToolStart};
use crate::config::{Config, KNOWN_MODELS};
use crate::session::{self, Session};
use crate::theme::Theme;
use crate::ui::{self, ShellSink, CANCEL};
use serde_json::{json, Value};
use std::io::Write as _;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<OutputFormat> {
        match value {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            "stream-json" | "stream_json" | "ndjson" => Some(OutputFormat::StreamJson),
            _ => None,
        }
    }
}

const ASK_USAGE: &str =
    "usage: odei ask [--output-format text|json|stream-json] \"<prompt>\" (or pipe it in)";

/// Everything a turn produced that the agent does not already know: which
/// tools ran, and anything it said outside the answer.
#[derive(Default)]
struct RecordSink {
    tools: Vec<Value>,
    notices: Vec<String>,
}

impl Sink for RecordSink {
    fn on_waiting(&mut self, _step: usize) {}
    fn on_text_delta(&mut self, _text: &str) {}
    fn on_text_done(&mut self) {}
    fn on_group_start(&mut self, _summary: &str) {}
    fn on_tool_start(&mut self, _start: &ToolStart) {}

    fn on_tool_done(&mut self, done: &ToolDone) {
        self.tools.push(json!({
            "tool": done.tool,
            "label": done.label,
            "ms": done.elapsed.as_millis(),
            "error": done.is_error,
            "call": done.call,
        }));
    }

    fn on_notice(&mut self, text: &str) {
        self.notices.push(text.to_string());
    }

    fn request_approval(&mut self, tool: &str, label: &str, _detail: &str) -> Approval {
        self.notices.push(format!(
            "denied {label} ({tool}): nobody here to approve it"
        ));
        Approval::Deny
    }
}

pub fn parse_ask_args(args: &[String]) -> Result<(OutputFormat, String), String> {
    let mut format = OutputFormat::Text;
    let mut words: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if let Some(value) = arg.strip_prefix("--output-format=") {
            format = OutputFormat::parse(value).ok_or_else(|| unknown_format(value))?;
        } else if arg == "--output-format" {
            let value = args.get(index + 1).ok_or_else(|| unknown_format(""))?;
            format = OutputFormat::parse(value).ok_or_else(|| unknown_format(value))?;
            index += 1;
        } else if arg == "--json" {
            format = OutputFormat::Json;
        } else {
            words.push(arg);
        }
        index += 1;
    }
    Ok((format, words.join(" ").trim().to_string()))
}

fn unknown_format(value: &str) -> String {
    format!("--output-format takes text, json, or stream-json, not {value:?}")
}

/// Read stdin when it is not a terminal, so a pipe reaches the prompt.
pub fn read_piped_stdin() -> Option<String> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer).ok()?;
    (!buffer.trim().is_empty()).then_some(buffer)
}

pub fn compose_prompt(typed: &str, piped: Option<String>) -> Result<String, String> {
    let typed = typed.trim();
    match (typed.is_empty(), piped) {
        (true, None) => Err(ASK_USAGE.to_string()),
        (true, Some(input)) => Ok(input.trim().to_string()),
        (false, None) => Ok(typed.to_string()),
        (false, Some(input)) => Ok(format!(
            "{typed}\n\n---\n\nPiped to odei on stdin:\n\n{}",
            input.trim_end()
        )),
    }
}

fn turn_report(
    agent: &Agent,
    model: &str,
    elapsed: std::time::Duration,
    result: &Result<(), String>,
) -> Value {
    json!({
        "ok": result.is_ok(),
        "error": result.as_ref().err(),
        "result": ui::last_assistant_text(agent).unwrap_or_default(),
        "session_id": agent.session.meta.id,
        "model": model,
        "turns": agent.turns,
        "ms": elapsed.as_millis(),
        "usage": {
            "input_tokens": agent.total_usage.input_tokens,
            "output_tokens": agent.total_usage.output_tokens,
            "cache_read_tokens": agent.total_usage.cache_read_tokens,
            "cache_write_tokens": agent.total_usage.cache_write_tokens,
            "context_tokens": agent.last_input_tokens,
        },
    })
}

fn report_failure(format: OutputFormat, message: &str) -> i32 {
    match format {
        OutputFormat::Text => eprintln!("odei: {message}"),
        OutputFormat::Json => {
            let body = json!({"ok": false, "error": message});
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::StreamJson => {
            println!(
                "{}",
                json!({"event": "result", "ok": false, "error": message})
            );
        }
    }
    1
}

pub fn ask(config: Config, prompt: &str, format: OutputFormat) -> i32 {
    if config.api_key.is_none() {
        return report_failure(format, crate::provider::MISSING_KEY_HINT);
    }
    ui::install_sigint_handler();
    let model = config.model.clone();
    let session = Session::create(&config.workspace_root, &config.model);
    let mut agent = Agent::new(config, session);
    CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    let started = std::time::Instant::now();

    match format {
        OutputFormat::Text => {
            let theme = Theme::detect();
            let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let detail = agent.config.detail;
            let mut sink = ShellSink::new(theme, interactive, detail);
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
        OutputFormat::Json => {
            let mut sink = RecordSink::default();
            let result = agent.run_user_turn(prompt, &CANCEL, &mut sink);
            let mut report = turn_report(&agent, &model, started.elapsed(), &result);
            report["tools"] = json!(sink.tools);
            report["notices"] = json!(sink.notices);
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
            i32::from(result.is_err())
        }
        OutputFormat::StreamJson => {
            let mut sink = crate::serve::detached_json_sink();
            let result = agent.run_user_turn(prompt, &CANCEL, &mut sink);
            let mut report = turn_report(&agent, &model, started.elapsed(), &result);
            report["event"] = json!("result");
            println!("{report}");
            i32::from(result.is_err())
        }
    }
}

pub fn setup_flow(provider: Option<&str>) -> Result<(), String> {
    let provider = match provider {
        None => crate::config::Provider::Kimi,
        Some(name) => crate::config::Provider::parse(name)
            .ok_or_else(|| format!("unknown provider: {name} (kimi or gemini)"))?,
    };
    match provider {
        crate::config::Provider::Kimi => {
            println!("Set up Kimi Code access.");
            println!(
                "Create an API key in the Kimi Code Console (kimi.com/code), then paste it here."
            );
        }
        crate::config::Provider::Gemini => {
            println!("Set up Gemini access.");
            println!("Create an API key in Google AI Studio (aistudio.google.com/apikey), then paste it here.");
        }
    }
    print!("API key: ");
    let _ = std::io::stdout().flush();
    let mut key = String::new();
    std::io::stdin()
        .read_line(&mut key)
        .map_err(|e| e.to_string())?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("no key entered".into());
    }
    let mut stored = crate::config::load_stored();
    match provider {
        crate::config::Provider::Kimi => stored.api_key = Some(key),
        crate::config::Provider::Gemini => stored.gemini_api_key = Some(key),
    }
    crate::config::save_stored(&stored).map_err(|e| e.to_string())?;
    println!(
        "Saved to {}",
        crate::config::odei_home().join("config.json").display()
    );
    Ok(())
}

pub fn status(config: &Config) -> i32 {
    println!("odei {}", env!("CARGO_PKG_VERSION"));
    println!("workspace   {}", config.workspace_root.display());
    println!("provider    {}", config.provider.label());
    println!("model       {}", config.model);
    println!("base url    {}", config.base_url);
    println!("key source  {}", config.key_source);
    println!("permissions {}", config.permission_mode.label());
    println!(
        "cache       {}",
        if config.prompt_cache { "on" } else { "off" }
    );
    println!("profile     {}", crate::config::odei_home().display());
    0
}

pub fn doctor(config: &Config) -> i32 {
    let mut failures = 0;
    let check = |ok: bool, label: &str, detail: &str| {
        println!(
            "{} {label}{}",
            if ok { "✓" } else { "✗" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
        !ok as i32
    };
    failures += check(config.api_key.is_some(), "API key", config.key_source);
    failures += check(
        crate::config::odei_home().exists()
            || std::fs::create_dir_all(crate::config::odei_home()).is_ok(),
        "profile directory",
        &crate::config::odei_home().display().to_string(),
    );
    failures += check(
        config.workspace_root.exists(),
        "workspace",
        &config.workspace_root.display().to_string(),
    );
    if config.api_key.is_some() {
        let label = format!("{} connectivity", config.provider.label());
        match crate::provider::check_connectivity(config) {
            Ok(()) => {
                failures += check(true, &label, &config.base_url);
            }
            Err(e) => {
                failures += check(false, &label, &e.to_string());
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
    let command = format!(
        "curl -fsSL --proto '=https' --tlsv1.2 {} | sh",
        installer_url(&latest)
    );
    if which("curl").is_none() || which("sh").is_none() {
        println!("Needs curl and sh. Install it by hand:\n  {command}");
        return 1;
    }
    println!("Running the installer published with {latest}:\n  {command}\n");
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .status()
    {
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
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
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
        println!(
            "{}  {age}  {title}  {} messages  {}",
            s.id, s.messages, s.workspace
        );
    }
    0
}

pub fn help() -> i32 {
    println!("odei — tiny native coding agent for the terminal, powered by Kimi or Gemini");
    println!();
    println!("usage:");
    println!("  odei                       interactive session in the current workspace");
    println!(
        "  odei ask \"<prompt>\"        single noninteractive request; stdin joins the prompt"
    );
    println!("       [--output-format text|json|stream-json]");
    println!("                             text (default), one JSON report, or NDJSON events");
    println!("  odei sessions              list saved sessions");
    println!("  odei session resume last   resume the latest session for this workspace");
    println!("  odei session resume --id <id>");
    println!("  odei serve [--workspace <dir>] [--resume last|<id>]");
    println!("                             headless NDJSON agent on stdio (drives the macOS app)");
    println!("  odei eval [name…] [--list] run the behavioural evals in ./evals/cases");
    println!("  odei setup [kimi|gemini]   store an API key (Kimi Code by default)");
    println!("  odei upgrade [--check]     update to the latest release");
    println!("  odei status                show runtime configuration");
    println!("  odei doctor                check configuration and connectivity");
    println!("  odei models                list known models");
    println!("  odei version               show the odei version");
    println!();
    println!("environment: KIMI_API_KEY, GEMINI_API_KEY, ODEI_PROVIDER, ODEI_MODEL,");
    println!("             ODEI_BASE_URL, ODEI_PERMISSIONS, ODEI_MAX_AGENT_STEPS,");
    println!("             ODEI_PROMPT_CACHE, ODEI_SYSTEM_PROMPT_FILE, ODEI_EVAL_DIR");
    0
}
