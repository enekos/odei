//! Headless mode: the same agent loop, spoken as NDJSON over stdio.
//!
//! `odei serve` exists so a front end that is not a terminal — the SwiftUI
//! app in `ui/` — can drive the real agent instead of reimplementing it or
//! scraping a pty. One JSON object per line in each direction, nothing else
//! on stdout, so a reader never has to guess where a record ends.
//!
//! Threading follows the shape of the loop. The agent runs on the main
//! thread and blocks: while it is streaming, waiting on the network, or
//! sitting inside a tool, it cannot poll stdin. So a reader thread owns
//! stdin and splits what it finds by urgency — `cancel` flips the shared
//! flag and `approve` goes down a channel the sink is already blocked on
//! (both must land *during* a turn), while everything else is queued for
//! the main loop to pick up when it is next idle.

use crate::agent::{Agent, Approval, Sink};
use crate::calls;
use crate::config::{Config, KNOWN_MODELS};
use crate::provider::ContentBlock;
use crate::session::{self, Session};
use crate::tools;
use crate::ui::CANCEL;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};

/// Write one event. Every line the front end reads comes through here.
fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Commands that need the agent, queued until the main loop is free.
enum Command {
    Prompt(String),
    Compact,
    Sessions,
    Calls,
    Call(usize),
    Model(String),
    Mode(String),
    Exit,
}

struct Answer {
    id: u64,
    approval: Approval,
}

/// Sink that renders to NDJSON instead of a terminal. Approvals block the
/// agent thread on `answers` until the front end replies.
struct JsonSink {
    answers: Receiver<Answer>,
    next_approval: u64,
    /// Open text block: `text_end` is only meaningful after some delta.
    streaming: bool,
}

impl Sink for JsonSink {
    fn on_waiting(&mut self, step: usize) {
        emit(json!({"event": "waiting", "step": step}));
    }

    fn on_thinking(&mut self, text: &str) {
        emit(json!({"event": "thinking", "delta": text}));
    }

    fn on_step_done(&mut self, step: &crate::agent::StepDone) {
        emit(json!({
            "event": "step",
            "step": step.step,
            "ms": step.elapsed.as_millis(),
            "input_tokens": step.usage.context_tokens(),
            "output_tokens": step.usage.output_tokens,
            "cached_tokens": step.usage.cache_read_tokens,
            "context_fraction": step.context_fraction,
            "tool_calls": step.tool_calls,
        }));
    }

    fn on_text_delta(&mut self, text: &str) {
        self.streaming = true;
        emit(json!({"event": "text", "delta": text}));
    }

    fn on_text_done(&mut self) {
        if self.streaming {
            self.streaming = false;
            emit(json!({"event": "text_end"}));
        }
    }

    fn on_group_start(&mut self, summary: &str) {
        emit(json!({"event": "group", "summary": summary}));
    }

    fn on_tool_start(&mut self, start: &crate::agent::ToolStart) {
        emit(json!({
            "event": "tool",
            "phase": "start",
            "tool": start.tool,
            "label": start.label,
            "qualifier": crate::activity::qualifier(start.tool, start.input),
            "last": start.last_in_group,
        }));
    }

    fn on_tool_done(&mut self, done: &crate::agent::ToolDone) {
        // The front end gets the same three things the shell draws — label,
        // stat, diff — so it can render a call without parsing tool output.
        let call = crate::activity::Call::of(done);
        emit(json!({
            "event": "tool",
            "phase": "done",
            "tool": done.tool,
            "label": done.label,
            "qualifier": crate::activity::qualifier(done.tool, done.input),
            "stat": call.stat(),
            "ms": done.elapsed.as_millis(),
            "error": done.is_error,
            "last": done.last_in_group,
            "call": done.call,
            "diff": done.diff,
        }));
    }

    fn on_notice(&mut self, text: &str) {
        emit(json!({"event": "notice", "text": text}));
    }

    fn request_approval(&mut self, tool: &str, label: &str, detail: &str) -> Approval {
        self.next_approval += 1;
        let id = self.next_approval;
        emit(json!({
            "event": "approval",
            "id": id,
            "tool": tool,
            "label": label,
            "detail": detail,
        }));
        // An answer to an earlier prompt (a double-click, a race with a
        // cancel) must not decide this one, so mismatched ids are dropped.
        loop {
            match self.answers.recv() {
                Ok(answer) if answer.id == id => return answer.approval,
                Ok(_) => continue,
                // The front end is gone; denying is the safe way to unwind.
                Err(_) => return Approval::Deny,
            }
        }
    }
}

/// The same NDJSON event stream, with nobody on the other end to answer an
/// approval — what `odei ask --output-format stream-json` writes. The answer
/// channel is closed from the start, so anything that stops for permission is
/// denied instead of hanging.
pub fn detached_json_sink() -> impl Sink {
    let (_closed, answers) = channel();
    JsonSink { answers, next_approval: 0, streaming: false }
}

/// Read commands forever, routing the two that must be handled mid-turn.
fn read_stdin(commands: Sender<Command>, answers: Sender<Answer>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            emit(json!({"event": "error", "text": format!("not JSON: {line}")}));
            continue;
        };
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        let queued = match value["cmd"].as_str().unwrap_or_default() {
            // Handled here, not queued: the agent is busy when these arrive.
            "cancel" => {
                CANCEL.store(true, Ordering::Relaxed);
                continue;
            }
            "approve" => {
                let approval = match value["answer"].as_str().unwrap_or("deny") {
                    "allow" => Approval::Allow,
                    "always" => Approval::AlwaysAllow,
                    _ => Approval::Deny,
                };
                let id = value["id"].as_u64().unwrap_or(0);
                let _ = answers.send(Answer { id, approval });
                continue;
            }
            "prompt" => Command::Prompt(text("text")),
            "compact" => Command::Compact,
            "sessions" => Command::Sessions,
            "calls" => Command::Calls,
            "call" => Command::Call(value["n"].as_u64().unwrap_or(0) as usize),
            "model" => Command::Model(text("value")),
            "mode" => Command::Mode(text("value")),
            "exit" => Command::Exit,
            other => {
                emit(json!({"event": "error", "text": format!("unknown command: {other}")}));
                continue;
            }
        };
        if commands.send(queued).is_err() {
            break;
        }
    }
    // stdin closed: the front end quit, so should we.
    let _ = commands.send(Command::Exit);
}

/// One transcript message, flattened for display. The stored user turn
/// carries the runtime-context preamble the model needs and the reader does
/// not, so it is cut back to what the person actually typed.
fn history_item(message: &crate::provider::Message) -> Option<Value> {
    let mut text = String::new();
    let mut tools_used: Vec<String> = Vec::new();
    let mut only_results = true;
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => {
                only_results = false;
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::ToolUse { name, input, .. } => {
                only_results = false;
                let label = tools::find(name)
                    .map(|spec| tools::activity_label(spec, input, true))
                    .unwrap_or_else(|| name.clone());
                tools_used.push(label);
            }
            // Tool results are the model's business, not the reader's.
            ContentBlock::ToolResult { .. } => {}
        }
    }
    if only_results {
        return None;
    }
    if message.role == "user" {
        if let Some(cut) = text.find("\n\n---\n\n") {
            text = text[cut + 7..].to_string();
        }
    }
    if text.trim().is_empty() && tools_used.is_empty() {
        return None;
    }
    Some(json!({"role": message.role, "text": text, "tools": tools_used}))
}

fn emit_state(agent: &Agent) {
    emit(json!({
        "event": "state",
        "session": agent.session.meta.id,
        "workspace": agent.config.workspace_root.display().to_string(),
        "model": agent.config.model,
        "mode": agent.config.permission_mode.label(),
        "context": agent.context_fraction(),
        "input_tokens": agent.total_usage.input_tokens,
        "output_tokens": agent.total_usage.output_tokens,
        "turns": agent.turns,
    }));
}

pub fn serve(mut config: Config, workspace: Option<String>, resume: Option<String>) -> i32 {
    if let Some(root) = workspace {
        let root = std::path::PathBuf::from(root);
        if !root.is_dir() {
            emit(json!({"event": "fatal", "text": format!("no such directory: {}", root.display())}));
            return 1;
        }
        // Reload rather than patch: the config is per-workspace.
        config = Config::load(&root);
    }
    if config.api_key.is_none() {
        emit(json!({
            "event": "fatal",
            "text": crate::provider::MISSING_KEY_HINT,
        }));
        return 1;
    }

    let resumed = match resume.as_deref() {
        Some("last") => session::latest_for_workspace(&config.workspace_root),
        Some(id) => Some(id.to_string()),
        None => None,
    };
    let session = match resumed {
        Some(id) => match Session::open(&id) {
            Some(session) => session,
            None => {
                emit(json!({"event": "fatal", "text": format!("no such session: {id}")}));
                return 1;
            }
        },
        None => Session::create(&config.workspace_root, &config.model),
    };

    let models: Vec<Value> =
        KNOWN_MODELS.iter().map(|(id, note)| json!({"id": id, "note": note})).collect();
    emit(json!({
        "event": "ready",
        "version": env!("CARGO_PKG_VERSION"),
        "key_source": config.key_source,
        "context_window": config.context_window(),
        "models": models,
    }));

    let mut agent = Agent::new(config, session);
    let history: Vec<Value> = agent.session.messages.iter().filter_map(history_item).collect();
    if !history.is_empty() {
        emit(json!({"event": "history", "items": history}));
    }
    emit_state(&agent);

    let (tx_command, rx_command) = channel::<Command>();
    let (tx_answer, rx_answer) = channel::<Answer>();
    std::thread::spawn(move || read_stdin(tx_command, tx_answer));
    let mut sink = JsonSink { answers: rx_answer, next_approval: 0, streaming: false };

    while let Ok(command) = rx_command.recv() {
        match command {
            Command::Exit => break,
            Command::Prompt(text) => {
                if text.trim().is_empty() {
                    continue;
                }
                CANCEL.store(false, Ordering::Relaxed);
                let result = agent.run_user_turn(&text, &CANCEL, &mut sink);
                sink.on_text_done();
                // A first prompt names the session, so `odei sessions` and
                // the picker have something better than "(untitled)".
                if agent.session.meta.title.is_none() {
                    let title: String = text.trim().chars().take(60).collect();
                    agent.session.rename(&title);
                }
                match result {
                    Ok(()) => emit(json!({"event": "turn_end", "ok": true})),
                    Err(e) => emit(json!({"event": "turn_end", "ok": false, "error": e})),
                }
                emit_state(&agent);
            }
            Command::Compact => {
                CANCEL.store(false, Ordering::Relaxed);
                match agent.compact_now(&CANCEL, &mut sink) {
                    Ok(report) => emit(json!({"event": "notice", "text": report})),
                    Err(e) => emit(json!({"event": "notice", "text": format!("could not compact: {e}")})),
                }
                emit_state(&agent);
            }
            Command::Sessions => {
                let items: Vec<Value> = session::list()
                    .into_iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "title": s.title,
                            "workspace": s.workspace,
                            "messages": s.messages,
                            "modified": chrono::DateTime::<chrono::Local>::from(s.modified)
                                .format("%Y-%m-%d %H:%M")
                                .to_string(),
                        })
                    })
                    .collect();
                emit(json!({"event": "sessions", "items": items}));
            }
            Command::Calls => {
                let items: Vec<Value> = calls::load(&agent.session.meta.id)
                    .iter()
                    .map(|r| {
                        json!({
                            "n": r.n,
                            "tool": r.tool,
                            "label": r.label,
                            "ms": r.ms,
                            "error": r.is_error,
                            "at": r.at,
                            "bytes": r.output.len(),
                        })
                    })
                    .collect();
                emit(json!({"event": "calls", "items": items}));
            }
            Command::Call(n) => {
                match calls::load(&agent.session.meta.id).into_iter().find(|r| r.n == n) {
                    // The same report the terminal pane shows, so the two
                    // surfaces can never drift.
                    Some(record) => emit(json!({
                        "event": "call",
                        "n": n,
                        "tool": record.tool,
                        "label": record.label,
                        "error": record.is_error,
                        "report": calls::report(&record, 72),
                    })),
                    None => emit(json!({"event": "error", "text": format!("no call #{n}")})),
                }
            }
            Command::Model(value) => {
                if KNOWN_MODELS.iter().any(|(id, _)| *id == value) || !value.is_empty() {
                    agent.config.model = value;
                    emit_state(&agent);
                }
            }
            Command::Mode(value) => {
                match crate::config::PermissionMode::parse(&value) {
                    Some(mode) => {
                        agent.config.permission_mode = mode;
                        emit_state(&agent);
                    }
                    None => emit(json!({"event": "error", "text": format!("unknown mode: {value}")})),
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn history_strips_the_runtime_context_preamble() {
        let message = Message::user_text("cwd: /tmp\nbranch: master\n\n---\n\nfix the parser");
        let item = history_item(&message).expect("user turn is shown");
        assert_eq!(item["text"], "fix the parser");
        assert_eq!(item["role"], "user");
    }

    #[test]
    fn history_names_tools_and_drops_result_only_turns() {
        let assistant = Message {
            role: "assistant".into(),
            content: vec![
                ContentBlock::Text { text: "Looking now.".into() },
                ContentBlock::ToolUse {
                    id: "1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/main.rs"}),
                },
            ],
        };
        let item = history_item(&assistant).expect("assistant turn is shown");
        assert_eq!(item["text"], "Looking now.");
        assert_eq!(item["tools"][0].as_str().unwrap(), "Read src/main.rs");

        // The synthetic user turn carrying tool results is not conversation.
        let results = Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "1".into(),
                content: "fn main() {}".into(),
                is_error: false,
            }],
        };
        assert!(history_item(&results).is_none());
    }
}
