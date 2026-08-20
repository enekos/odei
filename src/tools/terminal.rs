//! Terminal tool: `exec` runs a foreground command with one captured result;
//! `start` launches a durable session with incremental `read`, stdin `write`,
//! `wait`, `signal`, and `close`. Sessions run through the user's shell
//! (profile=user) or a bare `sh -c` (profile=clean).

use super::{ToolContext, ToolOutcome};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EXEC_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const EXEC_MAX_TIMEOUT_MS: u64 = 600_000;
const OUTPUT_CAP: usize = 48 * 1024;

struct Session {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
    read_cursor: usize,
    command: String,
}

#[derive(Default)]
pub struct TerminalRegistry {
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
}

fn shell_invocation(command: &str, profile: &str) -> Command {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut invocation = if profile == "clean" {
        let mut c = Command::new("/bin/sh");
        c.arg("-c");
        c
    } else {
        let mut c = Command::new(&shell);
        // Login shell so user startup files load (profile=user).
        c.arg("-lc");
        c
    };
    invocation.arg(command);
    invocation
}

fn truncate_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= OUTPUT_CAP {
        return text.into_owned();
    }
    let tail_start = text.len() - OUTPUT_CAP;
    let boundary = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= tail_start)
        .unwrap_or(tail_start);
    format!("[output truncated to final {OUTPUT_CAP} bytes]\n{}", &text[boundary..])
}

fn pump(reader: impl Read + Send + 'static, sink: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut sink = sink.lock().unwrap();
                    sink.extend_from_slice(&buf[..n]);
                    // Bound memory: keep at most 4x the reported cap.
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
        "close" => close(ctx, input),
        other => ToolOutcome::err(format!("unknown terminal action: {other:?}")),
    }
}

fn exec(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(command) = input["command"].as_str() else {
        return ToolOutcome::err("exec requires command");
    };
    let cwd = input["cwd"].as_str().map(|c| ctx.resolve(c)).unwrap_or_else(|| ctx.workspace_root.clone());
    let timeout = Duration::from_millis(
        input["timeout_ms"]
            .as_u64()
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_MS)
            .min(EXEC_MAX_TIMEOUT_MS),
    );
    let profile = input["profile"].as_str().unwrap_or("user");

    let mut invocation = shell_invocation(command, profile);
    invocation.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match invocation.spawn() {
        Ok(child) => child,
        Err(e) => return ToolOutcome::err(format!("cannot spawn command: {e}")),
    };

    let output = Arc::new(Mutex::new(Vec::new()));
    pump(child.stdout.take().unwrap(), output.clone());
    pump(child.stderr.take().unwrap(), output.clone());

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return ToolOutcome::err(format!("wait failed: {e}")),
        }
    };
    // Give the pump threads a beat to flush remaining output.
    std::thread::sleep(Duration::from_millis(30));
    let captured = truncate_tail(&output.lock().unwrap());

    match status {
        Some(status) => {
            let code = status.code().unwrap_or(-1);
            let text = if captured.trim().is_empty() {
                format!("(no output)\nexit code: {code}")
            } else {
                format!("{captured}\nexit code: {code}")
            };
            if status.success() {
                ToolOutcome::ok(text)
            } else {
                ToolOutcome::err(text)
            }
        }
        None => ToolOutcome::err(format!(
            "{captured}\ncommand timed out after {}ms and was killed; use action=start for long-running commands",
            timeout.as_millis()
        )),
    }
}

fn start(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let command = input["command"].as_str().unwrap_or_default();
    let cwd = input["cwd"].as_str().map(|c| ctx.resolve(c)).unwrap_or_else(|| ctx.workspace_root.clone());
    let profile = input["profile"].as_str().unwrap_or("user");

    let mut invocation = if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut c = Command::new(shell);
        c.arg("-i");
        c
    } else {
        shell_invocation(command, profile)
    };
    invocation.current_dir(&cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match invocation.spawn() {
        Ok(child) => child,
        Err(e) => return ToolOutcome::err(format!("cannot start session: {e}")),
    };

    let output = Arc::new(Mutex::new(Vec::new()));
    pump(child.stdout.take().unwrap(), output.clone());
    pump(child.stderr.take().unwrap(), output.clone());
    let stdin = child.stdin.take();

    let id = format!("terminal-{}", ctx.terminal.next_id.fetch_add(1, Ordering::Relaxed) + 1);
    let session = Session {
        child,
        stdin,
        output,
        read_cursor: 0,
        command: if command.is_empty() { "(interactive shell)".into() } else { command.into() },
    };
    ctx.terminal.sessions.lock().unwrap().insert(id.clone(), session);
    ToolOutcome::ok(format!(
        "started session {id} running {}; use action=read with this session_id for output",
        if command.is_empty() { "an interactive shell" } else { command }
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

fn session_status(session: &mut Session) -> String {
    match session.child.try_wait() {
        Ok(Some(status)) => format!("exited with code {}", status.code().unwrap_or(-1)),
        Ok(None) => "running".into(),
        Err(_) => "unknown".into(),
    }
}

fn read(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    match with_session(ctx, input, |session| {
        let output = session.output.lock().unwrap();
        let fresh = &output[session.read_cursor.min(output.len())..];
        let text = truncate_tail(fresh);
        session.read_cursor = output.len();
        drop(output);
        let status = session_status(session);
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
    match with_session(ctx, input, |session| match session.stdin.as_mut() {
        Some(stdin) => match stdin.write_all(text.as_bytes()).and_then(|()| stdin.flush()) {
            Ok(()) => ToolOutcome::ok(format!("wrote {} bytes to session stdin", text.len())),
            Err(e) => ToolOutcome::err(format!("cannot write to session: {e}")),
        },
        None => ToolOutcome::err("session stdin is closed"),
    }) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    }
}

fn wait(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let ceiling = Duration::from_millis(
        input["wait_ceiling_ms"].as_u64().unwrap_or(30_000).min(EXEC_MAX_TIMEOUT_MS),
    );
    match with_session(ctx, input, |session| {
        let start = Instant::now();
        loop {
            match session.child.try_wait() {
                Ok(Some(status)) => {
                    return ToolOutcome::ok(format!(
                        "session exited with code {}",
                        status.code().unwrap_or(-1)
                    ))
                }
                Ok(None) => {
                    if start.elapsed() > ceiling {
                        return ToolOutcome::ok(format!(
                            "still running after {}ms wait ceiling",
                            ceiling.as_millis()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return ToolOutcome::err(format!("wait failed: {e}")),
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
            let status = session_status(session);
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
        let pid = session.child.id() as i32;
        let result = unsafe { libc::kill(pid, signo) };
        if result == 0 {
            ToolOutcome::ok(format!("delivered {name} to session"))
        } else {
            ToolOutcome::err(format!("kill failed: {}", std::io::Error::last_os_error()))
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
            let output = session.output.lock().unwrap();
            let tail = truncate_tail(&output[session.read_cursor.min(output.len())..]);
            if tail.trim().is_empty() {
                ToolOutcome::ok(format!("closed session {id}"))
            } else {
                ToolOutcome::ok(format!("closed session {id}; final unread output:\n{tail}"))
            }
        }
        None => ToolOutcome::err(format!("unknown session_id: {id}")),
    }
}
