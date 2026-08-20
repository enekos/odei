//! Interactive shell: closer to a Unix shell than an IDE-in-the-terminal
//! TUI. Welcome line, dim statusline, `❯ ` input with
//! slash commands, streamed assistant text, `●`/`├`/`└` tool activity lines,
//! and a y/a/n approval prompt for sensitive actions.

use crate::agent::{Agent, Approval, Sink};
use crate::config::{Config, PermissionMode, KNOWN_MODELS};
use crate::provider::ContentBlock;
use crate::session::{self, Session};
use crate::theme::{self, Theme};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

/// Left margin for anything nested under a header line (a tool tree line,
/// an approval detail/button/decision line). Header lines (●, odei:,
/// streamed text) sit at column 0; nested detail sits one step in.
const INDENT: &str = "  ";

pub static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

pub fn install_sigint_handler() {
    unsafe {
        let handler = on_sigint as extern "C" fn(libc::c_int);
        libc::signal(libc::SIGINT, handler as usize as libc::sighandler_t);
    }
}

pub struct ShellSink<'t> {
    theme: &'t Theme,
    waiting_line: bool,
    tool_line_open: bool,
    printed_text: bool,
    interactive: bool,
}

impl<'t> ShellSink<'t> {
    pub fn new(theme: &'t Theme, interactive: bool) -> Self {
        ShellSink { theme, waiting_line: false, tool_line_open: false, printed_text: false, interactive }
    }

    fn clear_transient(&mut self) {
        if self.waiting_line {
            print!("\r\x1b[2K");
            self.waiting_line = false;
        }
    }

    fn finish_tool_line(&mut self) {
        if self.tool_line_open {
            println!();
            self.tool_line_open = false;
        }
    }
}

impl Sink for ShellSink<'_> {
    fn on_waiting(&mut self) {
        if !self.interactive {
            return;
        }
        self.clear_transient();
        print!("{}{}…{}", self.theme.dim, theme::ASK_ACTIVITY_LABEL, self.theme.reset());
        let _ = std::io::stdout().flush();
        self.waiting_line = true;
    }

    fn on_text_delta(&mut self, text: &str) {
        self.clear_transient();
        self.finish_tool_line();
        if !self.printed_text {
            self.printed_text = true;
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn on_text_done(&mut self) {
        self.clear_transient();
        if self.printed_text {
            println!();
            println!();
            self.printed_text = false;
        }
    }

    fn on_group_start(&mut self, summary: &str) {
        self.clear_transient();
        self.finish_tool_line();
        println!("{}● {summary}{}", self.theme.dim, self.theme.reset());
    }

    fn on_tool_start(&mut self, label: &str, last_in_group: bool) {
        self.clear_transient();
        self.finish_tool_line();
        if !self.interactive {
            // No TTY: skip the in-progress line; only completed lines print.
            return;
        }
        let connector = if last_in_group { "└" } else { "├" };
        print!("{}{INDENT}{connector} {label}{}", self.theme.dim, self.theme.reset());
        let _ = std::io::stdout().flush();
        self.tool_line_open = true;
    }

    fn on_tool_done(&mut self, label: &str, is_error: bool, last_in_group: bool) {
        let connector = if last_in_group { "└" } else { "├" };
        let style = if is_error { self.theme.warning } else { self.theme.dim };
        let marker = if is_error { " ✗" } else { "" };
        if self.tool_line_open {
            print!("\r\x1b[2K");
            self.tool_line_open = false;
        }
        println!("{style}{INDENT}{connector} {label}{marker}{}", self.theme.reset());
        if last_in_group {
            // Breathing room before whatever comes next: closing text,
            // the next tool group, or the statusline.
            println!();
        }
    }

    fn on_notice(&mut self, text: &str) {
        self.clear_transient();
        self.finish_tool_line();
        println!(
            "{}odei:{} {}{text}{}",
            self.theme.system_notice_label,
            self.theme.reset(),
            self.theme.system_notice_text,
            self.theme.reset()
        );
    }

    fn request_approval(&mut self, tool: &str, label: &str, detail: &str) -> Approval {
        self.clear_transient();
        self.finish_tool_line();
        if !self.interactive {
            return Approval::Deny;
        }
        println!();
        println!(
            "{}● Approval required{} {}({tool}){}",
            self.theme.subtitle,
            self.theme.reset(),
            self.theme.dim,
            self.theme.reset()
        );
        println!("{INDENT}{}{label}{}", self.theme.hint, self.theme.reset());
        let preview: String = detail.lines().take(12).collect::<Vec<_>>().join("\n");
        if !preview.trim().is_empty() && preview.trim() != "{}" {
            for line in preview.lines() {
                println!("{INDENT}{}{line}{}", self.theme.dim, self.theme.reset());
            }
        }
        println!();
        println!(
            "{INDENT}{} y {}  allow    {} a {}  always allow    {} n {}  deny",
            self.theme.approval_button_active,
            self.theme.reset(),
            self.theme.approval_button_inactive,
            self.theme.reset(),
            self.theme.approval_button_inactive,
            self.theme.reset()
        );
        let decision = read_approval_key();
        let text = match decision {
            Approval::Allow => "allowed once",
            Approval::AlwaysAllow => "always allowed (rule saved)",
            Approval::Deny => "denied",
        };
        println!("{INDENT}{}└ {text}{}", self.theme.dim, self.theme.reset());
        println!();
        decision
    }
}

fn read_approval_key() -> Approval {
    if crossterm::terminal::enable_raw_mode().is_err() {
        // No TTY control: fall back to a line read.
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return match line.trim() {
            "y" | "Y" | "yes" => Approval::Allow,
            "a" | "A" => Approval::AlwaysAllow,
            _ => Approval::Deny,
        };
    }
    let decision = loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => break Approval::Allow,
                KeyCode::Char('a') | KeyCode::Char('A') => break Approval::AlwaysAllow,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => break Approval::Deny,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Approval::Deny
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break Approval::Deny,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    decision
}

fn git_branch(workspace: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!branch.is_empty()).then_some(branch)
    } else {
        None
    }
}

fn statusline(theme: &Theme, agent: &Agent) -> String {
    let mode = agent.config.permission_mode;
    let mode_label = match mode {
        PermissionMode::Ask => "ask".to_string(),
        PermissionMode::Auto => format!("{}auto{}", theme.permission_auto, theme.statusline),
        PermissionMode::Yolo => format!("{}YOLO{}", theme.permission_auto, theme.statusline),
    };
    let workspace = agent
        .config
        .workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| agent.config.workspace_root.display().to_string());
    let mut segments = vec![mode_label, agent.config.model.clone(), workspace];
    if let Some(branch) = git_branch(&agent.config.workspace_root) {
        segments.push(branch);
    }
    let fraction = agent.context_fraction();
    if fraction > 0.0 {
        segments.push(format!("{:.0}% ctx", (fraction * 100.0).min(100.0)));
    }
    if let Some(title) = &agent.session.meta.title {
        segments.push(title.clone());
    }
    format!("{}{}{}", theme.statusline, segments.join(" · "), theme.reset())
}

const HELP: &[(&str, &str)] = &[
    ("/help", "list these commands"),
    ("/clear", "wipe the screen and begin a new session"),
    ("/new", "begin a new session, keeping this one saved"),
    ("/reset", "forget this session's history but keep the session"),
    ("/resume", "pick a saved session to continue"),
    ("/rename <title>", "give this session a name"),
    ("/model <id-or-query>", "switch model, and remember the choice"),
    ("/models", "list the models this key can reach"),
    ("/permissions [ask|auto|yolo|reset]", "decide how much runs without asking"),
    ("/allowlist", "show the approvals you told me to remember"),
    ("/status", "where I am pointed and how I am configured"),
    ("/stats", "turns and tokens for this session"),
    ("/usage (/cost)", "token totals per model across all sessions"),
    ("/compact", "summarize older turns to free up context"),
    ("/copy", "put my last reply on the clipboard"),
    ("/setup", "store a Kimi API key"),
    ("/version", "print the version"),
    ("/quit", "leave"),
];

fn print_help(theme: &Theme) {
    for (command, description) in HELP {
        println!(
            "{}{:<38}{}{}{description}{}",
            theme.hint,
            command,
            theme.reset(),
            theme.dim,
            theme.reset()
        );
    }
}

fn last_assistant_text(agent: &Agent) -> Option<String> {
    agent.session.messages.iter().rev().find_map(|message| {
        if message.role != "assistant" {
            return None;
        }
        let text: String = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    })
}


pub fn run_interactive(config: Config, resume: Option<String>) -> i32 {
    let theme = Theme::detect();
    install_sigint_handler();

    let session = match resume {
        Some(id) => match Session::open(&id) {
            Some(session) => session,
            None => {
                eprintln!("odei: unknown session: {id}");
                return 1;
            }
        },
        None => Session::create(&config.workspace_root, &config.model),
    };
    let resumed = !session.messages.is_empty();
    let mut agent = Agent::new(config, session);

    print!("{}", theme::welcome_message(theme, env!("CARGO_PKG_VERSION")));
    if agent.config.api_key.is_none() {
        println!(
            "{}No API key found. Run /setup or set KIMI_API_KEY.{}",
            theme.warning,
            theme.reset()
        );
    }
    if resumed {
        println!(
            "{}resumed session {} ({} messages){}",
            theme.dim,
            agent.session.meta.id,
            agent.session.messages.len(),
            theme.reset()
        );
    }
    println!();

    let mut editor = match rustyline::DefaultEditor::new() {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("odei: cannot initialize input: {e}");
            return 1;
        }
    };
    let history_path = crate::config::odei_home().join("history");
    let _ = editor.load_history(&history_path);

    loop {
        println!("{}", statusline(theme, &agent));
        let line = match editor.readline(theme::INPUT_PREFIX) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("{}(use /quit or Ctrl+D to exit){}", theme.dim, theme.reset());
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("odei: input error: {e}");
                break;
            }
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input);
        let _ = editor.save_history(&history_path);

        if let Some(rest) = input.strip_prefix('/') {
            let mut parts = rest.splitn(2, ' ');
            let command = parts.next().unwrap_or("");
            let arg = parts.next().unwrap_or("").trim();
            match command {
                "help" => print_help(theme),
                "quit" | "exit" => break,
                "version" => println!("odei {}", env!("CARGO_PKG_VERSION")),
                "clear" => {
                    print!("\x1b[2J\x1b[H");
                    agent.session = Session::create(&agent.config.workspace_root, &agent.config.model);
                    print!("{}", theme::welcome_message(theme, env!("CARGO_PKG_VERSION")));
                }
                "new" => {
                    agent.session = Session::create(&agent.config.workspace_root, &agent.config.model);
                    println!("{}started session {}{}", theme.dim, agent.session.meta.id, theme.reset());
                }
                "reset" => {
                    agent.session.messages.clear();
                    agent.session.rewrite();
                    println!("{}session context reset{}", theme.dim, theme.reset());
                }
                "resume" => {
                    let sessions = session::list();
                    if sessions.is_empty() {
                        println!("{}no saved sessions{}", theme.dim, theme.reset());
                        continue;
                    }
                    for (i, s) in sessions.iter().take(10).enumerate() {
                        let title = s.title.clone().unwrap_or_else(|| "(untitled)".into());
                        println!(
                            "{}{:>2}{} {} {}· {title} · {} messages · {}{}",
                            theme.hint,
                            i + 1,
                            theme.reset(),
                            s.id,
                            theme.dim,
                            s.messages,
                            s.workspace,
                            theme.reset()
                        );
                    }
                    if let Ok(choice) = editor.readline(&format!("{}resume #{} ", theme.dim, theme.reset())) {
                        if let Ok(n) = choice.trim().parse::<usize>() {
                            if n >= 1 && n <= sessions.len().min(10) {
                                if let Some(session) = Session::open(&sessions[n - 1].id) {
                                    println!(
                                        "{}resumed {} ({} messages){}",
                                        theme.dim,
                                        session.meta.id,
                                        session.messages.len(),
                                        theme.reset()
                                    );
                                    agent.session = session;
                                }
                            }
                        }
                    }
                }
                "rename" => {
                    if arg.is_empty() {
                        println!("{}usage: /rename <title>{}", theme.dim, theme.reset());
                    } else {
                        agent.session.rename(arg);
                        println!("{}renamed session to {arg:?}{}", theme.dim, theme.reset());
                    }
                }
                "model" => {
                    if arg.is_empty() {
                        println!("{}current model: {}{}", theme.dim, agent.config.model, theme.reset());
                        for (id, note) in KNOWN_MODELS {
                            println!("  {}{id}{} {}— {note}{}", theme.hint, theme.reset(), theme.dim, theme.reset());
                        }
                    } else {
                        let chosen = KNOWN_MODELS
                            .iter()
                            .find(|(id, _)| id.contains(arg))
                            .map(|(id, _)| id.to_string())
                            .unwrap_or_else(|| arg.to_string());
                        agent.config.model = chosen.clone();
                        let mut stored = crate::config::load_stored();
                        stored.model = Some(chosen.clone());
                        let _ = crate::config::save_stored(&stored);
                        println!("{}model set to {chosen}{}", theme.dim, theme.reset());
                    }
                }
                "models" => {
                    for (id, note) in KNOWN_MODELS {
                        let marker = if *id == agent.config.model { "●" } else { " " };
                        println!("{marker} {}{id}{} {}— {note}{}", theme.hint, theme.reset(), theme.dim, theme.reset());
                    }
                }
                "permissions" => match arg {
                    "" => {
                        println!(
                            "{}mode: {} · rules: {}{}",
                            theme.dim,
                            agent.config.permission_mode.label(),
                            agent.rules.rules.len(),
                            theme.reset()
                        );
                    }
                    "reset" => {
                        agent.rules.rules.clear();
                        crate::permissions::save_rules(&agent.rules);
                        println!("{}permission rules cleared{}", theme.dim, theme.reset());
                    }
                    other => match PermissionMode::parse(other) {
                        Some(mode) => {
                            agent.config.permission_mode = mode;
                            let mut stored = crate::config::load_stored();
                            stored.permissions = Some(mode);
                            let _ = crate::config::save_stored(&stored);
                            println!("{}permissions set to {}{}", theme.dim, mode.label(), theme.reset());
                        }
                        None => println!("{}usage: /permissions [ask|auto|yolo|reset]{}", theme.dim, theme.reset()),
                    },
                },
                "allowlist" => {
                    if agent.rules.rules.is_empty() {
                        println!("{}no saved rules{}", theme.dim, theme.reset());
                    }
                    for rule in &agent.rules.rules {
                        println!(
                            "{}{}{} {} {} {}{}{}",
                            theme.hint,
                            rule.id,
                            theme.reset(),
                            rule.effect,
                            rule.tool,
                            theme.dim,
                            rule.target,
                            theme.reset()
                        );
                    }
                }
                "status" => {
                    println!("workspace   {}", agent.config.workspace_root.display());
                    println!("model       {}", agent.config.model);
                    println!("base url    {}", agent.config.base_url);
                    println!("key source  {}", agent.config.key_source);
                    println!("permissions {}", agent.config.permission_mode.label());
                    println!("session     {}", agent.session.meta.id);
                    println!("profile     {}", crate::config::odei_home().display());
                }
                "stats" => {
                    println!(
                        "turns {} · input tokens {} · output tokens {}",
                        agent.turns, agent.total_usage.input_tokens, agent.total_usage.output_tokens
                    );
                    println!(
                        "context {} of {} tokens ({:.0}%)",
                        agent.last_input_tokens,
                        agent.config.context_window(),
                        agent.context_fraction() * 100.0
                    );
                }
                "usage" | "cost" => {
                    let path = crate::config::odei_home().join("usage.jsonl");
                    let mut by_model: std::collections::BTreeMap<String, (u64, u64, u64)> =
                        Default::default();
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        for line in text.lines() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                let entry = by_model
                                    .entry(v["model"].as_str().unwrap_or("?").to_string())
                                    .or_default();
                                entry.0 += 1;
                                entry.1 += v["input_tokens"].as_u64().unwrap_or(0);
                                entry.2 += v["output_tokens"].as_u64().unwrap_or(0);
                            }
                        }
                    }
                    if by_model.is_empty() {
                        println!("{}no recorded usage{}", theme.dim, theme.reset());
                    }
                    for (model, (requests, input, output)) in by_model {
                        println!("{model}: {requests} requests · {input} in · {output} out (subscription plan, no per-token spend)");
                    }
                }
                "compact" => {
                    CANCEL.store(false, Ordering::Relaxed);
                    let mut sink = ShellSink::new(theme, true);
                    match agent.compact_now(&CANCEL, &mut sink) {
                        Ok(report) => println!("{}{report}{}", theme.dim, theme.reset()),
                        Err(e) => println!("{}could not compact: {e}{}", theme.warning, theme.reset()),
                    }
                }
                "copy" => match last_assistant_text(&agent) {
                    Some(text) => {
                        let copied = std::process::Command::new("pbcopy")
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .and_then(|mut child| {
                                child.stdin.as_mut().unwrap().write_all(text.as_bytes())?;
                                child.wait()
                            })
                            .map(|status| status.success())
                            .unwrap_or(false);
                        if copied {
                            println!("{}copied last response{}", theme.dim, theme.reset());
                        } else {
                            println!("{}clipboard unavailable{}", theme.warning, theme.reset());
                        }
                    }
                    None => println!("{}nothing to copy yet{}", theme.dim, theme.reset()),
                },
                "setup" => {
                    if let Err(e) = crate::cli::setup_flow() {
                        println!("{}setup failed: {e}{}", theme.warning, theme.reset());
                    } else {
                        agent.config = Config::load(&agent.config.workspace_root);
                    }
                }
                other => println!(
                    "{}unknown command: /{other} — run /help{}",
                    theme.dim,
                    theme.reset()
                ),
            }
            continue;
        }

        // A model turn.
        CANCEL.store(false, Ordering::Relaxed);
        let mut sink = ShellSink::new(theme, true);
        if let Err(e) = agent.run_user_turn(input, &CANCEL, &mut sink) {
            println!("{}odei: {e}{}", theme.warning, theme.reset());
        }
    }

    let _ = editor.save_history(&history_path);
    0
}
