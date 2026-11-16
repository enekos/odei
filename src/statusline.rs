//! An external statusline: `ODEI_STATUSLINE` names a program that is handed
//! the session as JSON on stdin and prints the line to draw. The payload uses
//! Claude Code's `statusLine` field names, so one script serves both agents.
//!
//! The program is run without a shell, and anything that goes wrong — it is
//! missing, slow, exits non-zero, prints nothing — leaves the built-in line
//! in place. A statusline is never worth a stuck prompt.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::agent::Agent;

const ENV: &str = "ODEI_STATUSLINE";
/// Longer than this and the user feels the prompt lag.
const BUDGET: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(5);
/// Two rows of ANSI-coloured text fit many times over.
const MAX_OUTPUT: u64 = 16 * 1024;

pub fn render(agent: &Agent) -> Option<String> {
    let program = std::env::var(ENV).ok().filter(|p| !p.trim().is_empty())?;
    run(program.trim(), &payload(agent).to_string(), BUDGET)
}

fn payload(agent: &Agent) -> serde_json::Value {
    let fraction = agent.context_fraction();
    serde_json::json!({
        "session_id": agent.session.meta.id,
        "model": { "id": agent.config.model, "display_name": agent.config.model },
        "workspace": { "current_dir": agent.config.workspace_root },
        "context_window": {
            "used_percentage": (fraction > 0.0).then(|| (fraction * 100.0).min(100.0)),
        },
        "permission_mode": agent.config.permission_mode.label(),
    })
}

fn run(program: &str, input: &str, budget: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .env("AGENTLINE_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // A program that exits without reading stdin breaks the pipe; its output
    // is still what counts, so a failed write is not a failed statusline.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < budget => std::thread::sleep(POLL),
            _ => {
                // Already exiting or gone if these fail; nothing left to clean up.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(MAX_OUTPUT)
        .read_to_string(&mut out)
        .ok()?;
    let out = out.trim_end();
    (!out.is_empty()).then(|| out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(body: &str) -> tempfile_path::Script {
        tempfile_path::Script::new(body)
    }

    #[test]
    fn prints_what_the_program_prints() {
        let s = script("#!/bin/sh\ncat >/dev/null\nprintf 'one\\ntwo\\n'\n");
        assert_eq!(run(s.path(), "{}", BUDGET).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn the_payload_reaches_stdin() {
        let s = script("#!/bin/sh\ncat\n");
        assert_eq!(
            run(s.path(), "{\"a\":1}", BUDGET).as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn a_failing_program_falls_back() {
        let s = script("#!/bin/sh\necho partial\nexit 1\n");
        assert_eq!(run(s.path(), "{}", BUDGET), None);
    }

    #[test]
    fn a_slow_program_is_killed_and_falls_back() {
        let s = script("#!/bin/sh\nsleep 5\necho late\n");
        let started = Instant::now();
        assert_eq!(run(s.path(), "{}", Duration::from_millis(100)), None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_missing_program_falls_back() {
        assert_eq!(run("/nonexistent/odei-statusline", "{}", BUDGET), None);
    }

    #[test]
    fn empty_output_falls_back() {
        let s = script("#!/bin/sh\nexit 0\n");
        assert_eq!(run(s.path(), "{}", BUDGET), None);
    }

    mod tempfile_path {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);

        pub struct Script(PathBuf);

        impl Script {
            pub fn new(body: &str) -> Script {
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("odei-statusline-{}-{n}.sh", std::process::id()));
                std::fs::write(&path, body).expect("temp dir is writable");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("just created it");
                Script(path)
            }

            pub fn path(&self) -> &str {
                self.0.to_str().expect("temp paths are UTF-8 here")
            }
        }

        impl Drop for Script {
            fn drop(&mut self) {
                // Best effort: a leftover file in the temp dir harms nothing.
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }
}
