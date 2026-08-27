//! Interactive shell: closer to a Unix shell than an IDE-in-the-terminal
//! TUI. Splash, dim statusline, `❯ ` input with
//! slash commands, streamed assistant text, `●`/`├`/`└` tool activity lines,
//! and a y/a/n approval prompt for sensitive actions.
//!
//! How much a tool call shows is one setting, [`Detail`], and it has three
//! positions: `/collapse` for one line each, the default for one line plus
//! the diff a file-changing call made, and `/expand` for everything — the
//! arguments, the whole diff, the output, the model's reasoning as it
//! streams, and what each round trip cost. Output only ever grows downwards,
//! so calls already on the screen stay as they were drawn; `/expand N`
//! reprints one from the journal at full detail.

use crate::activity::{self, Call};
use crate::agent::{Agent, Approval, Sink, StepDone, ToolDone, ToolStart};
use crate::blinker::Blinker;
use crate::config::{Config, Detail, PermissionMode, KNOWN_MODELS};
use crate::context::NoteScope;
use crate::markdown;
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
/// Column at which a call's body sits: the margin, plus the tree bar and the
/// space after it.
const BODY_MARGIN: usize = INDENT.len() + 2;
/// Calls `/expand all` prints before it starts pointing at `/call N` instead.
const EXPAND_ALL_CAP: usize = 40;

/// Room to draw in. Wide terminals stop growing the measure at a readable
/// line length instead of stretching a diff across the whole desk.
fn width() -> usize {
    crossterm::terminal::size().map(|(columns, _)| columns as usize).unwrap_or(80).clamp(40, 160)
}

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
    theme: &'static Theme,
    /// The model writes markdown; this turns it into terminal output as it
    /// streams.
    markdown: markdown::Renderer<'t>,
    /// How much of each tool call to draw.
    detail: Detail,
    /// The subtle blinker under the "Thinking…" line, while the model works.
    blinker: Option<Blinker>,
    waiting_line: bool,
    tool_line_open: bool,
    printed_text: bool,
    /// Reasoning arrives in fragments; this holds the part of the current
    /// line that has not been terminated yet. `Some` means the thinking
    /// block is open.
    thinking: Option<String>,
    interactive: bool,
}

impl<'t> ShellSink<'t> {
    pub fn new(theme: &'static Theme, interactive: bool, detail: Detail) -> Self {
        ShellSink {
            theme,
            markdown: markdown::Renderer::new(theme),
            detail,
            blinker: None,
            waiting_line: false,
            tool_line_open: false,
            printed_text: false,
            thinking: None,
            interactive,
        }
    }

    fn clear_transient(&mut self) {
        if self.waiting_line {
            // The blinker sits below the waiting line; it must stop before
            // the line it hangs from is erased.
            if let Some(blinker) = &mut self.blinker {
                blinker.clear();
            }
            self.blinker = None;
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

    /// Close off any half-rendered markdown before something else prints.
    /// The agent ends a text run before it runs tools, so this is a
    /// backstop — but a stray tool line inside an open table would be
    /// unreadable.
    fn finish_markdown(&mut self) {
        if !self.markdown.idle() {
            print!("{}", self.markdown.finish());
            let _ = std::io::stdout().flush();
        }
    }

    /// Flush the tail of a reasoning block and close it.
    fn finish_thinking(&mut self) {
        if let Some(rest) = self.thinking.take() {
            let rest = rest.trim();
            if !rest.is_empty() {
                self.print_quoted(rest);
            }
            println!();
        }
    }

    fn print_quoted(&self, text: &str) {
        for line in quoted(self.theme, text, width()) {
            println!("{line}");
        }
    }

    /// The block under an activity line, hanging from the tree.
    fn print_body(&self, lines: Vec<String>, last_in_group: bool) {
        print_body(self.theme, lines, last_in_group)
    }
}

/// A stretch of the model's reasoning, in the margin, as a quotation — it is
/// the model talking to itself, not to the user. Reasoning is prose, so it is
/// wrapped to the measure rather than cut: losing the end of a thought is
/// worse than spending a second line on it.
fn quoted(theme: &Theme, text: &str, width: usize) -> Vec<String> {
    wrap(&activity::sanitize(text), width.saturating_sub(BODY_MARGIN))
        .into_iter()
        .map(|line| {
            format!(
                "{}{INDENT}│{} {}{line}{}",
                theme.quote_bar,
                theme.reset(),
                theme.quote,
                theme.reset()
            )
        })
        .collect()
}

/// Greedy word wrap. A word longer than the measure is left long rather than
/// broken — it is a path or an identifier, and breaking it helps nobody.
fn wrap(text: &str, room: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let width_with = current.chars().count() + 1 + word.chars().count();
        if current.is_empty() {
            current.push_str(word);
        } else if width_with <= room {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn print_body(theme: &Theme, lines: Vec<String>, last_in_group: bool) {
    let bar = if last_in_group { " " } else { "│" };
    for line in lines {
        println!("{}{INDENT}{bar}{} {line}", theme.quote_bar, theme.reset());
    }
}

/// A tool activity line: the tree connector, what is being done, what the
/// arguments add to that, and — once it is over — what came back, how long it
/// took, and the handle that reopens it.
#[allow(clippy::too_many_arguments)]
fn activity_line(
    theme: &Theme,
    width: usize,
    connector: &str,
    label: &str,
    qualifier: Option<String>,
    stat: Option<String>,
    timing: Option<String>,
    call: Option<usize>,
    is_error: bool,
) -> String {
    let (label_style, stat_style) =
        if is_error { (theme.warning, theme.warning) } else { (theme.dim, theme.hint) };
    let marker = if is_error { " ✗" } else { "" };
    let qualifier = qualifier.map(|q| format!(" · {q}")).unwrap_or_default();
    let right = match (&stat, &timing) {
        (Some(stat), Some(timing)) => format!("{stat} · {timing}"),
        (Some(stat), None) => stat.clone(),
        (None, Some(timing)) => timing.clone(),
        (None, None) => String::new(),
    };
    let handle = call.map(|n| format!("#{n}")).unwrap_or_default();

    // Everything but the head is short and load-bearing, so the head is what
    // gives way when the terminal is narrow.
    let fixed = INDENT.len() + 2 + marker.len() + gap(&right) + right.chars().count()
        + gap(&handle) + handle.chars().count();
    let head =
        activity::clip(&format!("{label}{qualifier}"), width.saturating_sub(fixed).max(12));
    let mut line = format!("{label_style}{INDENT}{connector} {head}{marker}{}", theme.reset());
    if !right.is_empty() {
        line.push_str(&format!("  {stat_style}{right}{}", theme.reset()));
    }
    if !handle.is_empty() {
        line.push_str(&format!("  {}{handle}{}", theme.hint, theme.reset()));
    }
    line
}

/// Two spaces before a segment, or nothing when the segment is empty.
fn gap(segment: &str) -> usize {
    if segment.is_empty() {
        0
    } else {
        2
    }
}

impl Sink for ShellSink<'_> {
    fn on_waiting(&mut self, step: usize) {
        if !self.interactive {
            return;
        }
        self.clear_transient();
        self.finish_thinking();
        // Past the first round trip the step number is the only sign that
        // the agent is still working through something rather than stuck.
        let step = if step > 1 && self.detail.shows_steps() {
            format!(" {}· step {step}", self.theme.dim)
        } else {
            String::new()
        };
        print!(
            "{}{}…{step}{}",
            self.theme.dim,
            theme::ASK_ACTIVITY_LABEL,
            self.theme.reset()
        );
        let _ = std::io::stdout().flush();
        self.waiting_line = true;
        println!();
        self.blinker = Blinker::maybe_start(self.theme, true);
    }

    fn on_thinking(&mut self, text: &str) {
        if !self.interactive || !self.detail.shows_thinking() {
            return;
        }
        self.clear_transient();
        if self.thinking.is_none() {
            self.finish_tool_line();
            println!("{}⏺ thinking{}", self.theme.dim, self.theme.reset());
            self.thinking = Some(String::new());
        }
        let mut ready: Vec<String> = Vec::new();
        if let Some(buffer) = self.thinking.as_mut() {
            buffer.push_str(text);
            // Reasoning arrives mid-word; only whole lines can be given a
            // margin, so the rest waits here for the fragment that ends it.
            while let Some(end) = buffer.find('\n') {
                let line: String = buffer.drain(..=end).collect();
                let line = line.trim();
                if !line.is_empty() {
                    ready.push(line.to_string());
                }
            }
        }
        for line in ready {
            self.print_quoted(&line);
        }
        let _ = std::io::stdout().flush();
    }

    fn on_step_done(&mut self, step: &StepDone) {
        if !self.interactive || !self.detail.shows_steps() {
            return;
        }
        self.clear_transient();
        self.finish_thinking();
        self.finish_tool_line();
        let mut parts = vec![format!(
            "{} in · {} out",
            activity::tokens(step.usage.context_tokens()),
            activity::tokens(step.usage.output_tokens)
        )];
        if step.usage.cache_read_tokens > 0 {
            parts.push(format!("{} cached", activity::tokens(step.usage.cache_read_tokens)));
        }
        if let Some(timing) = activity::duration(step.elapsed.as_millis() as u64) {
            parts.push(timing);
        }
        // Below a percent there is nothing to worry about, and "0% ctx" on
        // every step is just a wider line.
        if step.context_fraction >= 0.01 {
            parts.push(format!("{:.0}% ctx", (step.context_fraction * 100.0).min(100.0)));
        }
        println!("{}{INDENT}· {}{}", self.theme.dim, parts.join(" · "), self.theme.reset());
    }

    fn on_text_delta(&mut self, text: &str) {
        self.clear_transient();
        self.finish_thinking();
        self.finish_tool_line();
        self.printed_text = true;
        print!("{}", self.markdown.push(text));
        let _ = std::io::stdout().flush();
    }

    fn on_text_done(&mut self) {
        self.clear_transient();
        print!("{}", self.markdown.finish());
        if self.printed_text {
            // The renderer ends its last line; this is the breathing room
            // before the statusline or the next group.
            println!();
            self.printed_text = false;
        }
        let _ = std::io::stdout().flush();
    }

    fn on_group_start(&mut self, summary: &str) {
        self.clear_transient();
        self.finish_thinking();
        self.finish_markdown();
        self.finish_tool_line();
        println!("{}● {summary}{}", self.theme.dim, self.theme.reset());
    }

    fn on_tool_start(&mut self, start: &ToolStart) {
        self.clear_transient();
        self.finish_thinking();
        self.finish_tool_line();
        if !self.interactive {
            // No TTY: skip the in-progress line; only completed lines print.
            return;
        }
        let connector = if start.last_in_group { "└" } else { "├" };
        print!(
            "{}",
            activity_line(
                self.theme,
                width(),
                connector,
                start.label,
                activity::qualifier(start.tool, start.input),
                None,
                None,
                None,
                false,
            )
        );
        let _ = std::io::stdout().flush();
        self.tool_line_open = true;
    }

    fn on_tool_done(&mut self, done: &ToolDone) {
        let connector = if done.last_in_group { "└" } else { "├" };
        if self.tool_line_open {
            print!("\r\x1b[2K");
            self.tool_line_open = false;
        }
        let call = Call::of(done);
        // The handle is what /call takes, and what /calls lists.
        println!(
            "{}",
            activity_line(
                self.theme,
                width(),
                connector,
                done.label,
                activity::qualifier(done.tool, done.input),
                call.stat(),
                activity::duration(done.elapsed.as_millis() as u64),
                done.call,
                done.is_error,
            )
        );
        self.print_body(
            call.body(self.theme, self.detail, BODY_MARGIN, width()),
            done.last_in_group,
        );
        if done.last_in_group {
            // Breathing room before whatever comes next: closing text,
            // the next tool group, or the statusline.
            println!();
        }
    }

    fn on_notice(&mut self, text: &str) {
        self.clear_transient();
        self.finish_thinking();
        self.finish_markdown();
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
        self.finish_thinking();
        self.finish_markdown();
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

fn workspace_name(agent: &Agent) -> String {
    agent
        .config
        .workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| agent.config.workspace_root.display().to_string())
}

/// The dim line under the splash wordmark: what this session is pointed at.
fn splash_subtitle(agent: &Agent) -> String {
    format!("{} · {}", agent.config.model, workspace_name(agent))
}

fn splash(theme: &Theme, agent: &Agent) {
    crate::splash::show(theme, env!("CARGO_PKG_VERSION"), &splash_subtitle(agent));
}

fn statusline(theme: &Theme, agent: &Agent) -> String {
    let mode = agent.config.permission_mode;
    let mode_label = match mode {
        PermissionMode::Ask => "ask".to_string(),
        PermissionMode::Auto => format!("{}auto{}", theme.permission_auto, theme.statusline),
        PermissionMode::Yolo => format!("{}YOLO{}", theme.permission_auto, theme.statusline),
    };
    let mut segments = vec![mode_label, agent.config.model.clone(), workspace_name(agent)];
    // Only when it is not the default: a statusline that names every setting
    // stops being read.
    if agent.config.detail != Detail::Normal {
        segments.push(agent.config.detail.label().to_string());
    }
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
    ("@<path>", "attach a file — or a directory's map — to your message"),
    ("!<command>", "run a command yourself; I see it with your next message"),
    ("#<note>", "remember something in AGENTS.md"),
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
    ("/calls", "pick a tool call and see exactly what it did"),
    ("/call <n>", "open call #n — command, arguments, full output"),
    ("/expand", "show every tool call in full: arguments, diffs, output"),
    ("/expand <n|last|all>", "reprint calls already made, in full, here"),
    ("/collapse", "fold tool calls back to one line each"),
    ("/detail [level]", "collapsed, normal, or expanded — and remember it"),
    ("/compact", "summarize older turns to free up context"),
    ("/copy", "put my last reply on the clipboard"),
    ("/setup", "store a Kimi API key"),
    ("/splash", "watch the wordmark condense again"),
    ("/version", "print the version"),
    ("/quit", "leave"),
];

fn print_help(theme: &Theme, workspace: &std::path::Path) {
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
    let user_commands = crate::commands::list(workspace);
    if user_commands.is_empty() {
        println!();
        println!(
            "{}your own commands go in .odei/commands/<name>.md (this project) or {}/commands/<name>.md{}",
            theme.dim,
            crate::config::odei_home().display(),
            theme.reset()
        );
        return;
    }
    println!();
    for command in user_commands {
        let description = command
            .description
            .unwrap_or_else(|| command.body.lines().next().unwrap_or("").to_string());
        println!(
            "{}{:<38}{}{}{} · {}{}",
            theme.hint,
            format!("/{}", command.name),
            theme.reset(),
            theme.dim,
            activity::clip(&description, 60),
            command.scope.label(),
            theme.reset()
        );
    }
}

/// The names `/`-completion offers, taken from the same table `/help` prints.
pub fn builtin_command_names() -> Vec<&'static str> {
    HELP.iter()
        .filter_map(|(command, _)| command.strip_prefix('/'))
        .map(|command| command.split([' ', '(']).next().unwrap_or(command))
        .collect()
}

/// Cache accounting for /stats: how much of the prompt came back from cache
/// rather than being read again, and why it might be zero.
fn cache_line(agent: &Agent) -> String {
    let usage = agent.total_usage;
    let served = usage.cache_read_tokens;
    let prompt = usage.input_tokens + usage.cache_write_tokens + served;
    let state = if !agent.config.prompt_cache {
        " (off)"
    } else if crate::provider::cache_rejected() {
        " (endpoint rejected cache_control; disabled)"
    } else {
        ""
    };
    let share = if prompt > 0 { served as f64 / prompt as f64 * 100.0 } else { 0.0 };
    format!(
        "prompt cache{state} · {served} read · {} written · {share:.0}% of prompt served from cache",
        usage.cache_write_tokens
    )
}

/// Change how much of a tool call is drawn, and remember it for next time.
fn set_detail(theme: &Theme, agent: &mut Agent, level: Detail) {
    agent.config.detail = level;
    let mut stored = crate::config::load_stored();
    stored.detail = Some(level);
    let _ = crate::config::save_stored(&stored);
    let note = match level {
        Detail::Collapsed => "one line per tool call",
        Detail::Normal => "one line per call, plus the diff when a file changes",
        Detail::Expanded => "arguments, diffs, output, reasoning, per-step cost",
    };
    println!("{}detail: {} — {note}{}", theme.dim, level.label(), theme.reset());
    if level == Detail::Expanded {
        println!(
            "{}calls already on screen keep the shape they were drawn in — /expand last to redraw them{}",
            theme.dim,
            theme.reset()
        );
    }
}

/// `/expand N`, `/expand last [k]` and `/expand all`: draw calls that have
/// already happened at full detail, here, without opening a pane. Terminal
/// output cannot be rewritten in place, so expanding history means printing
/// it again.
fn reprint_calls(theme: &Theme, session_id: &str, arg: &str) {
    let records = crate::calls::load(session_id);
    if records.is_empty() {
        println!("{}no tool calls in this session yet{}", theme.dim, theme.reset());
        return;
    }
    let mut words = arg.split_whitespace();
    let which = words.next().unwrap_or("last");
    let count = words.next().and_then(|value| value.parse::<usize>().ok());
    let chosen: Vec<&crate::calls::Record> = match which {
        "all" => {
            let from = records.len().saturating_sub(EXPAND_ALL_CAP);
            if from > 0 {
                println!(
                    "{}{from} earlier call{} not shown — /call N opens any of them{}",
                    theme.dim,
                    if from == 1 { "" } else { "s" },
                    theme.reset()
                );
            }
            records[from..].iter().collect()
        }
        "last" => {
            let count = count.unwrap_or(5).min(records.len());
            records[records.len() - count..].iter().collect()
        }
        number => match number.trim_start_matches('#').parse::<usize>() {
            Ok(n) => match records.iter().find(|record| record.n == n) {
                Some(record) => vec![record],
                None => {
                    println!(
                        "{}no call #{n} in this session (calls are #1–#{}){}",
                        theme.dim,
                        records.last().map(|r| r.n).unwrap_or(0),
                        theme.reset()
                    );
                    return;
                }
            },
            Err(_) => {
                println!(
                    "{}usage: /expand <n> · /expand last [k] · /expand all{}",
                    theme.dim,
                    theme.reset()
                );
                return;
            }
        },
    };
    let width = width();
    for (index, record) in chosen.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let call = Call::of_record(record);
        println!(
            "{}",
            activity_line(
                theme,
                width,
                "└",
                &record.label,
                activity::qualifier(&record.tool, &record.input),
                call.stat(),
                activity::duration(record.ms),
                Some(record.n),
                record.is_error,
            )
        );
        print_body(theme, call.body(theme, Detail::Expanded, BODY_MARGIN, width), true);
    }
}

/// A command the user ran with `!`, drawn like any other call — except that
/// they asked for the output, so it is shown whatever the detail level says.
fn print_shell_run(theme: &Theme, run: &crate::agent::ShellRun) {
    const OUTPUT_LINES: usize = 40;
    let call = Call {
        tool: "terminal",
        input: &run.input,
        output: &run.output,
        is_error: run.is_error,
        diff: None,
        call: Some(run.call),
    };
    println!(
        "{}",
        activity_line(
            theme,
            width(),
            "└",
            &run.label,
            activity::qualifier("terminal", &run.input),
            call.stat(),
            activity::duration(run.elapsed.as_millis() as u64),
            Some(run.call),
            run.is_error,
        )
    );
    let lines: Vec<&str> = run.output.lines().collect();
    let shown: Vec<&str> = if lines.len() <= OUTPUT_LINES {
        lines.clone()
    } else {
        lines[lines.len() - OUTPUT_LINES..].to_vec()
    };
    let elided = lines.len() - shown.len();
    let room = width().saturating_sub(BODY_MARGIN);
    let mut body: Vec<String> = Vec::new();
    if elided > 0 {
        body.push(format!(
            "{}… {elided} earlier lines · /call {}{}",
            theme.dim,
            run.call,
            theme.reset()
        ));
    }
    for line in shown {
        body.push(format!("{}{}{}", theme.dim, activity::clip(line, room), theme.reset()));
    }
    print_body(theme, body, true);
    println!();
}

/// `#note` — where should this be remembered?
fn read_note_scope() -> Option<NoteScope> {
    if crossterm::terminal::enable_raw_mode().is_err() {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return match line.trim() {
            "p" | "P" | "" => Some(NoteScope::Project),
            "g" | "G" => Some(NoteScope::Global),
            _ => None,
        };
    }
    let chosen = loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) => match key.code {
                KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Enter => {
                    break Some(NoteScope::Project)
                }
                KeyCode::Char('g') | KeyCode::Char('G') => break Some(NoteScope::Global),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    chosen
}

fn remember_flow(theme: &Theme, workspace: &std::path::Path, note: &str) {
    let note = note.trim();
    if note.is_empty() {
        println!(
            "{}usage: #<note> — writes it to AGENTS.md, where I read it every turn{}",
            theme.dim,
            theme.reset()
        );
        return;
    }
    println!("{}● Remember{}", theme.subtitle, theme.reset());
    println!("{INDENT}{}{note}{}", theme.hint, theme.reset());
    println!();
    println!(
        "{INDENT}{} p {}  this project    {} g {}  everywhere    {} esc {}  cancel",
        theme.approval_button_active,
        theme.reset(),
        theme.approval_button_inactive,
        theme.reset(),
        theme.approval_button_inactive,
        theme.reset()
    );
    let Some(scope) = read_note_scope() else {
        println!("{INDENT}{}└ not saved{}", theme.dim, theme.reset());
        println!();
        return;
    };
    match crate::context::remember(workspace, scope, note) {
        Ok(path) => {
            println!("{INDENT}{}└ saved to {}{}", theme.dim, path.display(), theme.reset())
        }
        Err(e) => println!("{INDENT}{}└ could not save: {e}{}", theme.warning, theme.reset()),
    }
    println!();
}

pub fn last_assistant_text(agent: &Agent) -> Option<String> {
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

    splash(theme, &agent);
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

    let mut editor: rustyline::Editor<
        crate::complete::OdeiHelper,
        rustyline::history::DefaultHistory,
    > = match rustyline::Editor::new() {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("odei: cannot initialize input: {e}");
            return 1;
        }
    };
    // Tab completes slash commands at the start of a line and paths after @.
    editor.set_helper(Some(crate::complete::OdeiHelper::new(
        &agent.config.workspace_root,
        builtin_command_names(),
    )));
    let history_path = crate::config::odei_home().join("history");
    let _ = editor.load_history(&history_path);
    let mut shell_hint_shown = false;

    loop {
        // The input block owns vertical room on both sides — a blank line
        // above the statusline and one below the typed line — so the user's
        // turn reads as its own block instead of running into the output
        // above and below it.
        println!();
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
        println!();

        if let Some(note) = input.strip_prefix('#') {
            remember_flow(theme, &agent.config.workspace_root, note);
            continue;
        }

        if let Some(command) = input.strip_prefix('!') {
            let command = command.trim();
            if command.is_empty() {
                println!(
                    "{}usage: !<command> — runs it here, and I see it with your next message{}",
                    theme.dim,
                    theme.reset()
                );
                continue;
            }
            let run = agent.run_shell(command);
            print_shell_run(theme, &run);
            if !shell_hint_shown {
                shell_hint_shown = true;
                println!(
                    "{}that command and its output ride along with your next message{}",
                    theme.dim,
                    theme.reset()
                );
            }
            continue;
        }

        // What the model is asked, once slash commands and @ mentions have
        // had their say. `None` means the line was handled here and there is
        // no turn to run.
        let mut queued: Option<String> = Some(input.to_string());

        if let Some(rest) = input.strip_prefix('/') {
            queued = None;
            let mut parts = rest.splitn(2, ' ');
            let command = parts.next().unwrap_or("");
            let arg = parts.next().unwrap_or("").trim();
            match command {
                "help" => print_help(theme, &agent.config.workspace_root),
                "quit" | "exit" => break,
                "version" => println!("odei {}", env!("CARGO_PKG_VERSION")),
                "clear" => {
                    print!("\x1b[2J\x1b[H");
                    agent.pending_context.clear();
                    agent.session = Session::create(&agent.config.workspace_root, &agent.config.model);
                    splash(theme, &agent);
                }
                "splash" => splash(theme, &agent),
                "new" => {
                    agent.pending_context.clear();
                    agent.session = Session::create(&agent.config.workspace_root, &agent.config.model);
                    println!("{}started session {}{}", theme.dim, agent.session.meta.id, theme.reset());
                }
                "reset" => {
                    agent.pending_context.clear();
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
                                    agent.pending_context.clear();
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
                    println!("detail      {}", agent.config.detail.label());
                    println!("session     {}", agent.session.meta.id);
                    println!("profile     {}", crate::config::odei_home().display());
                }
                "stats" => {
                    println!(
                        "turns {} · input tokens {} · output tokens {}",
                        agent.turns, agent.total_usage.input_tokens, agent.total_usage.output_tokens
                    );
                    println!("{}", cache_line(&agent));
                    println!(
                        "context {} of {} tokens ({:.0}%)",
                        agent.last_input_tokens,
                        agent.config.context_window(),
                        agent.context_fraction() * 100.0
                    );
                }
                "usage" | "cost" => {
                    let path = crate::config::odei_home().join("usage.jsonl");
                    let mut by_model: std::collections::BTreeMap<String, [u64; 4]> =
                        Default::default();
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        for line in text.lines() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                let entry = by_model
                                    .entry(v["model"].as_str().unwrap_or("?").to_string())
                                    .or_default();
                                entry[0] += 1;
                                entry[1] += v["input_tokens"].as_u64().unwrap_or(0);
                                entry[2] += v["output_tokens"].as_u64().unwrap_or(0);
                                entry[3] += v["cache_read_tokens"].as_u64().unwrap_or(0);
                            }
                        }
                    }
                    if by_model.is_empty() {
                        println!("{}no recorded usage{}", theme.dim, theme.reset());
                    }
                    for (model, [requests, input, output, cached]) in by_model {
                        println!("{model}: {requests} requests · {input} in · {output} out · {cached} from cache (subscription plan, no per-token spend)");
                    }
                }
                "calls" => crate::inspect::picker(theme, &agent.session.meta.id),
                "call" => {
                    // `#7` and `7` both address call seven.
                    match arg.trim_start_matches('#').parse::<usize>() {
                        Ok(n) if n > 0 => {
                            crate::inspect::show(theme, &agent.session.meta.id, n);
                        }
                        _ => println!(
                            "{}usage: /call <n> — or /calls to pick one{}",
                            theme.dim,
                            theme.reset()
                        ),
                    }
                }
                "expand" => {
                    if arg.is_empty() {
                        set_detail(theme, &mut agent, Detail::Expanded);
                    } else {
                        // With an argument it reopens history instead of
                        // changing what happens next.
                        reprint_calls(theme, &agent.session.meta.id, arg);
                    }
                }
                "collapse" => set_detail(theme, &mut agent, Detail::Collapsed),
                "detail" => match arg {
                    "" => println!(
                        "{}detail: {} — /collapse, /expand, or /detail <collapsed|normal|expanded>{}",
                        theme.dim,
                        agent.config.detail.label(),
                        theme.reset()
                    ),
                    other => match Detail::parse(other) {
                        Some(level) => set_detail(theme, &mut agent, level),
                        None => println!(
                            "{}usage: /detail [collapsed|normal|expanded]{}",
                            theme.dim,
                            theme.reset()
                        ),
                    },
                },
                "compact" => {
                    CANCEL.store(false, Ordering::Relaxed);
                    let mut sink = ShellSink::new(theme, true, agent.config.detail);
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
                other => match crate::commands::find(&agent.config.workspace_root, other) {
                    Some(command) => {
                        println!(
                            "{}/{} · {} command{}",
                            theme.dim,
                            command.name,
                            command.scope.label(),
                            theme.reset()
                        );
                        queued = Some(crate::commands::expand(&command.body, arg));
                    }
                    None => println!(
                        "{}unknown command: /{other} — run /help{}",
                        theme.dim,
                        theme.reset()
                    ),
                },
            }
        }

        let Some(turn_text) = queued else { continue };

        let (turn_text, attached) = crate::mentions::expand(&agent.tool_context, &turn_text);
        if !attached.is_empty() {
            let list: Vec<String> = attached
                .iter()
                .map(|item| format!("@{} ({}, {})", item.mention, item.kind, item.summary))
                .collect();
            println!("{}attached {}{}", theme.dim, list.join(" · "), theme.reset());
            println!();
        }

        // A model turn.
        CANCEL.store(false, Ordering::Relaxed);
        let mut sink = ShellSink::new(theme, true, agent.config.detail);
        if let Err(e) = agent.run_user_turn(&turn_text, &CANCEL, &mut sink) {
            println!("{}odei: {e}{}", theme.warning, theme.reset());
        }
    }

    let _ = editor.save_history(&history_path);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The measure is passed in, so these read the same at any window size.
    const W: usize = 72;

    #[test]
    fn an_activity_line_reads_label_then_what_came_back_then_the_handle() {
        let line = activity_line(
            theme::plain(),
            W,
            "├",
            "Read src/ui.rs",
            Some("lines 120–319".into()),
            Some("142 lines".into()),
            Some("1.2s".into()),
            Some(7),
            false,
        );
        assert_eq!(line, "  ├ Read src/ui.rs · lines 120–319  142 lines · 1.2s  #7");
    }

    #[test]
    fn a_running_call_has_no_stat_yet_and_a_failed_one_says_why() {
        let running =
            activity_line(theme::plain(), W, "└", "Editing a.rs", None, None, None, None, false);
        assert_eq!(running, "  └ Editing a.rs");

        let failed = activity_line(
            theme::plain(),
            W,
            "└",
            "Edited a.rs",
            None,
            Some("old_string not found in a.rs".into()),
            None,
            Some(3),
            true,
        );
        assert_eq!(failed, "  └ Edited a.rs ✗  old_string not found in a.rs  #3");
    }

    #[test]
    fn a_narrow_window_takes_it_out_of_the_label_not_the_stat() {
        let line = activity_line(
            theme::plain(),
            40,
            "├",
            "Ran cargo test --workspace --all-features",
            Some("in packages/core".into()),
            Some("exit 0".into()),
            Some("2.4s".into()),
            Some(12),
            false,
        );
        assert!(line.chars().count() <= 40, "{} chars: {line}", line.chars().count());
        assert!(line.ends_with("exit 0 · 2.4s  #12"), "{line}");
        assert!(line.contains('…'), "{line}");
    }

    #[test]
    fn an_edit_draws_its_diff_under_the_line() {
        let diff = crate::diff::compute("src/a.rs", "one\ntwo\nthree\n", "one\nTWO\nthree\n", false);
        let input = json!({"path": "src/a.rs"});
        let done = ToolDone {
            tool: "edit_file",
            label: "Edited src/a.rs",
            input: &input,
            output: "Edited src/a.rs at line 2",
            is_error: false,
            last_in_group: true,
            call: Some(4),
            elapsed: std::time::Duration::from_millis(30),
            diff: Some(&diff),
        };
        let call = Call::of(&done);
        let line = activity_line(
            theme::plain(),
            W,
            "└",
            done.label,
            activity::qualifier(done.tool, done.input),
            call.stat(),
            activity::duration(30),
            done.call,
            false,
        );
        // 30ms is not worth a timing, and the diff is the stat.
        assert_eq!(line, "  └ Edited src/a.rs  +1 −1  #4");
        let body = call.body(theme::plain(), Detail::Normal, BODY_MARGIN, W);
        assert_eq!(body, vec![" 1   one", " 2 - two", " 2 + TWO", " 3   three"]);
        // And folded away entirely when the user asked for one line each.
        assert!(call.body(theme::plain(), Detail::Collapsed, BODY_MARGIN, W).is_empty());
    }

    #[test]
    fn reasoning_is_quoted_in_the_margin_and_wrapped_not_cut() {
        assert_eq!(quoted(theme::plain(), "the file is small", W), ["  │ the file is small"]);
        // A long thought keeps every word, across as many lines as it needs.
        let thought = "the edit tool said line two but the file says line three, so the \
                       number it reports is off by one and worth checking";
        let lines = quoted(theme::plain(), thought, 40);
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|line| line.chars().count() <= 40), "{lines:?}");
        let rejoined: String =
            lines.iter().map(|line| line.trim_start_matches(['│', ' '])).collect::<Vec<_>>().join(" ");
        assert_eq!(rejoined, thought);
    }
}
