//! Terminal tool, backed by a real pseudo-terminal.
//!
//! `exec` runs a command to completion and returns its combined output plus
//! exit code; `start` leaves a session running that `read`, `write`, `wait`,
//! `signal`, `resize` and `close` then drive. Because the child gets a tty,
//! programs behave the way they do for a human at a prompt rather than
//! switching to their piped-output code path.

use super::{ToolContext, ToolOutcome};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EXEC_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const EXEC_MAX_TIMEOUT_MS: u64 = 600_000;
/// Generous: the tool-result store bounds what actually reaches the model, so
/// keeping more here means long build logs stay searchable.
const OUTPUT_CAP: usize = 256 * 1024;
const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    read_cursor: usize,
    command: String,
    exit: Option<u32>,
}

#[derive(Default)]
pub struct TerminalRegistry {
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
}

/// Turn raw tty bytes into something worth putting in a tool result: drop
/// escape sequences, and where a line was rewritten in place with carriage
/// returns (progress bars, spinners) keep only its final state.
fn sanitize(bytes: &[u8]) -> String {
    let raw = String::from_utf8_lossy(bytes);
    let mut plain = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: consume params, then the final byte in @..~
                Some('[') => {
                    for p in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&p) {
                            break;
                        }
                    }
                }
                // OSC: runs until BEL or ESC \
                Some(']') => {
                    while let Some(p) = chars.next() {
                        if p == '\x07' {
                            break;
                        }
                        if p == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Charset selection and similar two-byte sequences
                Some('(') | Some(')') | Some('#') => {
                    chars.next();
                }
                _ => {}
            },
            '\n' | '\t' | '\r' => plain.push(c),
            c if (c as u32) < 0x20 || c == '\x7f' => {}
            c => plain.push(c),
        }
    }

    plain
        .split('\n')
        .map(|line| {
            // CRLF first: the \r terminating a line is not an overwrite, and
            // treating it as one would discard the line entirely.
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line).trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_string()
}

fn bounded(text: String) -> String {
    if text.len() <= OUTPUT_CAP {
        return text;
    }
    let cut = text.len() - OUTPUT_CAP;
    let boundary = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= cut)
        .unwrap_or(cut);
    format!("[earlier output dropped]\n{}", &text[boundary..])
}

fn pty_size(rows: Option<u16>, cols: Option<u16>) -> PtySize {
    PtySize {
        rows: rows.unwrap_or(DEFAULT_ROWS),
        cols: cols.unwrap_or(DEFAULT_COLS),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn build_command(ctx: &ToolContext, input: &Value, interactive_default: bool) -> CommandBuilder {
    let command = input["command"].as_str().unwrap_or_default();
    let profile = input["profile"].as_str().unwrap_or("user");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());

    let mut cmd = if command.is_empty() && interactive_default {
        let mut c = CommandBuilder::new(shell);
        c.arg("-i");
        c
    } else if profile == "clean" {
        let mut c = CommandBuilder::new("/bin/sh");
        c.arg("-c");
        c.arg(command);
        c
    } else {
        // Login shell so the user's aliases, PATH and env are in play.
        let mut c = CommandBuilder::new(shell);
        c.arg("-lc");
        c.arg(command);
        c
    };

    let cwd = input["cwd"]
        .as_str()
        .map(|c| ctx.resolve(c))
        .unwrap_or_else(|| ctx.workspace_root.clone());
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    if !interactive_default {
        // A captured command that stops for a pager or a credential prompt
        // just burns its timeout, so take those paths away.
        cmd.env("PAGER", "cat");
        cmd.env("GIT_PAGER", "cat");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
    cmd
}

fn pump(mut reader: Box<dyn Read + Send>, sink: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut sink = sink.lock().unwrap();
                    sink.extend_from_slice(&buf[..n]);
                    let len = sink.len();
                    if len > OUTPUT_CAP * 4 {
                        sink.drain(..len - OUTPUT_CAP * 4);
                    }
                }
            }
        }
    });
}

pub fn terminal(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    match input["action"].as_str().unwrap_or("") {
        "exec" => exec(ctx, input),
        "start" => start(ctx, input),
        "read" => read(ctx, input),
        "write" => write(ctx, input),
        "wait" => wait(ctx, input),
        "list" => list(ctx),
        "signal" => signal(ctx, input),
        "resize" => resize(ctx, input),
        "close" => close(ctx, input),
        other => ToolOutcome::err(format!("unknown terminal action: {other:?}")),
    }
}

fn exec(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    if input["command"].as_str().map(str::is_empty).unwrap_or(true) {
        return ToolOutcome::err("exec requires a command");
    }
    let timeout = Duration::from_millis(
        input["timeout_ms"]
            .as_u64()
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_MS)
            .min(EXEC_MAX_TIMEOUT_MS),
    );

    let pair = match native_pty_system().openpty(pty_size(None, None)) {
        Ok(pair) => pair,
        Err(e) => return ToolOutcome::err(format!("cannot open a pty: {e}")),
    };
    let mut child = match pair.slave.spawn_command(build_command(ctx, input, false)) {
        Ok(child) => child,
        Err(e) => return ToolOutcome::err(format!("cannot start command: {e}")),
    };
    // Release the slave so the reader sees EOF once the child is gone.
    drop(pair.slave);

    let output = Arc::new(Mutex::new(Vec::new()));
    match pair.master.try_clone_reader() {
        Ok(reader) => pump(reader, output.clone()),
        Err(e) => return ToolOutcome::err(format!("cannot read from the pty: {e}")),
    }

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return ToolOutcome::err(format!("cannot wait on the command: {e}")),
        }
    };
    // Let the pump drain whatever was written just before exit.
    std::thread::sleep(Duration::from_millis(40));
    drop(pair.master);
    let captured = bounded(sanitize(&output.lock().unwrap()));

    match status {
        Some(status) => {
            let code = status.exit_code();
            let body =
                if captured.trim().is_empty() { "(no output)".to_string() } else { captured };
            let text = format!("{body}\nexit code: {code}");
            if status.success() {
                ToolOutcome::ok(text)
            } else {
                ToolOutcome::err(text)
            }
        }
        None => ToolOutcome::err(format!(
            "{captured}\ntimed out after {}ms and was killed. For something long-running or interactive, use action=start instead.",
            timeout.as_millis()
        )),
    }
}

fn start(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let rows = input["rows"].as_u64().map(|v| v as u16);
    let cols = input["columns"].as_u64().map(|v| v as u16);
    let pair = match native_pty_system().openpty(pty_size(rows, cols)) {
        Ok(pair) => pair,
        Err(e) => return ToolOutcome::err(format!("cannot open a pty: {e}")),
    };
    let child = match pair.slave.spawn_command(build_command(ctx, input, true)) {
        Ok(child) => child,
        Err(e) => return ToolOutcome::err(format!("cannot start session: {e}")),
    };
    drop(pair.slave);

    let output = Arc::new(Mutex::new(Vec::new()));
    match pair.master.try_clone_reader() {
        Ok(reader) => pump(reader, output.clone()),
        Err(e) => return ToolOutcome::err(format!("cannot read from the pty: {e}")),
    }
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(e) => return ToolOutcome::err(format!("cannot write to the pty: {e}")),
    };

    let raw_command = input["command"].as_str().unwrap_or_default();
    let id = format!(
        "terminal-{}",
        ctx.terminal.next_id.fetch_add(1, Ordering::Relaxed) + 1
    );
    let session = Session {
        child,
        master: pair.master,
        writer,
        output,
        read_cursor: 0,
        command: if raw_command.is_empty() {
            "(interactive shell)".into()
        } else {
            raw_command.into()
        },
        exit: None,
    };
    ctx.terminal
        .sessions
        .lock()
        .unwrap()
        .insert(id.clone(), session);
    ToolOutcome::ok(format!(
        "started session {id} running {}. Use action=read with this session_id to collect output.",
        if raw_command.is_empty() {
            "an interactive shell"
        } else {
            raw_command
        }
    ))
}

fn with_session<T>(
    ctx: &ToolContext,
    input: &Value,
    f: impl FnOnce(&mut Session) -> T,
) -> Result<T, ToolOutcome> {
    let Some(id) = input["session_id"].as_str() else {
        return Err(ToolOutcome::err("this action requires session_id"));
    };
    let mut sessions = ctx.terminal.sessions.lock().unwrap();
    match sessions.get_mut(id) {
        Some(session) => Ok(f(session)),
        None => Err(ToolOutcome::err(format!("unknown session_id: {id}"))),
    }
}

fn status_of(session: &mut Session) -> String {
    if let Some(code) = session.exit {
        return format!("exited with code {code}");
    }
    match session.child.try_wait() {
        Ok(Some(status)) => {
            let code = status.exit_code();
            session.exit = Some(code);
            format!("exited with code {code}")
        }
        Ok(None) => "running".into(),
        Err(_) => "unknown".into(),
    }
}

fn read(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    match with_session(ctx, input, |session| {
        let fresh = {
            let output = session.output.lock().unwrap();
            let from = session.read_cursor.min(output.len());
            let slice = output[from..].to_vec();
            session.read_cursor = output.len();
            slice
        };
        let text = bounded(sanitize(&fresh));
        let status = status_of(session);
        if text.trim().is_empty() {
            format!("(no new output)\nstatus: {status}")
        } else {
            format!("{text}\nstatus: {status}")
        }
    }) {
        Ok(text) => ToolOutcome::ok(text),
        Err(outcome) => outcome,
    }
}

fn write(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(text) = input["text"].as_str() else {
        return ToolOutcome::err("write requires text");
    };
    match with_session(ctx, input, |session| {
        match session
            .writer
            .write_all(text.as_bytes())
            .and_then(|()| session.writer.flush())
        {
            Ok(()) => ToolOutcome::ok(format!("wrote {} bytes to the session", text.len())),
            Err(e) => ToolOutcome::err(format!("cannot write to the session: {e}")),
        }
    }) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

fn wait(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let ceiling = Duration::from_millis(
        input["wait_ceiling_ms"]
            .as_u64()
            .unwrap_or(30_000)
            .min(EXEC_MAX_TIMEOUT_MS),
    );
    match with_session(ctx, input, |session| {
        if let Some(code) = session.exit {
            return ToolOutcome::ok(format!("session already exited with code {code}"));
        }
        let started = Instant::now();
        loop {
            match session.child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.exit_code();
                    session.exit = Some(code);
                    return ToolOutcome::ok(format!("session exited with code {code}"));
                }
                Ok(None) => {
                    if started.elapsed() > ceiling {
                        return ToolOutcome::ok(format!(
                            "still running after the {}ms ceiling",
                            ceiling.as_millis()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return ToolOutcome::err(format!("cannot wait on the session: {e}")),
            }
        }
    }) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

fn list(ctx: &ToolContext) -> ToolOutcome {
    let mut sessions = ctx.terminal.sessions.lock().unwrap();
    if sessions.is_empty() {
        return ToolOutcome::ok("no terminal sessions".to_string());
    }
    let mut rows: Vec<String> = sessions
        .iter_mut()
        .map(|(id, session)| {
            let status = status_of(session);
            format!("{id}: {} — {status}", session.command)
        })
        .collect();
    rows.sort();
    ToolOutcome::ok(rows.join("\n"))
}

fn signal(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let name = input["signal"].as_str().unwrap_or("terminate");
    let signo = match name {
        "hangup" => libc::SIGHUP,
        "interrupt" => libc::SIGINT,
        "quit" => libc::SIGQUIT,
        "terminate" => libc::SIGTERM,
        "kill" => libc::SIGKILL,
        other => return ToolOutcome::err(format!("unknown signal: {other:?}")),
    };
    match with_session(ctx, input, |session| {
        let Some(pid) = session.child.process_id() else {
            return ToolOutcome::err("session has no live process");
        };
        let pid = pid as i32;
        // The child leads its own process group under a pty, so signalling the
        // group reaches anything it spawned; fall back to the process itself.
        let delivered = unsafe { libc::kill(-pid, signo) == 0 || libc::kill(pid, signo) == 0 };
        if delivered {
            ToolOutcome::ok(format!("delivered {name}"))
        } else {
            ToolOutcome::err(format!(
                "could not signal: {}",
                std::io::Error::last_os_error()
            ))
        }
    }) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

fn resize(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let rows = input["rows"].as_u64().map(|v| v as u16);
    let cols = input["columns"].as_u64().map(|v| v as u16);
    if rows.is_none() && cols.is_none() {
        return ToolOutcome::err("resize requires rows and/or columns");
    }
    match with_session(ctx, input, |session| {
        let size = pty_size(rows, cols);
        match session.master.resize(size) {
            Ok(()) => ToolOutcome::ok(format!("resized to {}x{}", size.rows, size.cols)),
            Err(e) => ToolOutcome::err(format!("cannot resize: {e}")),
        }
    }) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

fn close(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(id) = input["session_id"].as_str() else {
        return ToolOutcome::err("close requires session_id");
    };
    let mut sessions = ctx.terminal.sessions.lock().unwrap();
    match sessions.remove(id) {
        Some(mut session) => {
            let _ = session.child.kill();
            let _ = session.child.wait();
            let tail = {
                let output = session.output.lock().unwrap();
                let from = session.read_cursor.min(output.len());
                bounded(sanitize(&output[from..]))
            };
            if tail.trim().is_empty() {
                ToolOutcome::ok(format!("closed session {id}"))
            } else {
                ToolOutcome::ok(format!(
                    "closed session {id}. Output you had not read:\n{tail}"
                ))
            }
        }
        None => ToolOutcome::err(format!("unknown session_id: {id}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_and_keeps_final_line_state() {
        let raw = b"\x1b[32mgreen\x1b[0m\r\nprogress 10%\rprogress 90%\rdone\n";
        assert_eq!(sanitize(raw), "green\ndone\n".trim_end_matches('\n'));
    }

    #[test]
    fn sanitize_drops_osc_titles() {
        let raw = b"\x1b]0;window title\x07visible";
        assert_eq!(sanitize(raw), "visible");
    }
}

#[cfg(test)]
mod pty_tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ToolContext {
        ToolContext::new(std::path::Path::new("/tmp"))
    }

    #[test]
    fn commands_run_on_a_real_terminal() {
        let out = terminal(
            &ctx(),
            &json!({"action": "exec", "command": "test -t 1 && echo IS_A_TTY || echo NOT_A_TTY"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("IS_A_TTY"),
            "stdout must be a tty: {}",
            out.text
        );
    }

    #[test]
    fn exit_codes_surface_as_errors() {
        let out = terminal(&ctx(), &json!({"action": "exec", "command": "exit 3"}));
        assert!(out.is_error);
        assert!(out.text.contains("exit code: 3"), "{}", out.text);
    }

    #[test]
    fn colour_and_progress_rewrites_are_cleaned_up() {
        let out = terminal(
            &ctx(),
            &json!({"action": "exec", "command": "printf '\\033[31mred\\033[0m\\n'; printf 'a\\rb\\rc\\n'"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("red"), "{}", out.text);
        assert!(
            !out.text.contains('\x1b'),
            "escapes should be gone: {:?}",
            out.text
        );
        assert!(out.text.contains('c'), "{}", out.text);
        assert!(
            !out.text.contains("a\rb"),
            "overwrites should collapse: {:?}",
            out.text
        );
    }

    #[test]
    fn a_session_takes_input_and_gives_back_output() {
        let ctx = ctx();
        let started = terminal(&ctx, &json!({"action": "start", "command": "cat"}));
        assert!(!started.is_error, "{}", started.text);
        let id = started
            .text
            .split("started session ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .expect("a session id")
            .to_string();

        let wrote = terminal(
            &ctx,
            &json!({"action": "write", "session_id": id, "text": "ping\n"}),
        );
        assert!(!wrote.is_error, "{}", wrote.text);
        std::thread::sleep(Duration::from_millis(300));

        let read = terminal(&ctx, &json!({"action": "read", "session_id": id}));
        assert!(
            read.text.contains("ping"),
            "session should echo input back: {}",
            read.text
        );
        assert!(read.text.contains("running"), "{}", read.text);

        let listed = terminal(&ctx, &json!({"action": "list"}));
        assert!(listed.text.contains(&id), "{}", listed.text);

        let closed = terminal(&ctx, &json!({"action": "close", "session_id": id}));
        assert!(!closed.is_error, "{}", closed.text);
        let gone = terminal(&ctx, &json!({"action": "read", "session_id": id}));
        assert!(gone.is_error, "closed sessions should be unknown");
    }

    #[test]
    fn high_volume_output_is_captured_whole() {
        let out = terminal(&ctx(), &json!({"action": "exec", "command": "seq 1 30000"}));
        assert!(!out.is_error, "{}", out.text);
        eprintln!("captured {} bytes", out.text.len());
        assert!(out.text.contains("\n30000\n"), "the tail must survive");
        assert!(
            out.text.len() > 150_000,
            "only captured {} bytes",
            out.text.len()
        );
    }

    #[test]
    fn a_hung_command_is_killed_at_its_timeout() {
        let out = terminal(
            &ctx(),
            &json!({"action": "exec", "command": "sleep 30", "timeout_ms": 700}),
        );
        assert!(out.is_error);
        assert!(out.text.contains("timed out"), "{}", out.text);
    }
}
