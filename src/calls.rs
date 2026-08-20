//! Per-call journal: what each tool call actually did, kept out of the
//! transcript so it can be inspected in full later.
//!
//! The conversation only ever sees a bounded tool result — capped by the
//! tool, then previewed again if it is over the inline budget. The journal
//! keeps the whole thing next to the arguments, the exit status, the timing,
//! and a shell command that reproduces the call, so `/calls` can show a
//! complete, copy-pasteable account of any step.

use crate::tools::ToolOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Per-record ceiling on stored output, matching the terminal capture cap.
const OUTPUT_CAP: usize = 256 * 1024;
/// Above this, a write_file body is described rather than inlined as a heredoc.
const HEREDOC_CAP: usize = 8 * 1024;
const PRUNE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Serialize, Deserialize)]
pub struct Record {
    /// 1-based index within the session; what `#N` refers to.
    pub n: usize,
    pub tool: String,
    /// The activity line the user saw, e.g. "Ran cargo test".
    pub label: String,
    pub input: Value,
    pub cwd: String,
    pub at: String,
    pub ms: u64,
    pub is_error: bool,
    pub output: String,
}

pub struct Journal {
    path: PathBuf,
    next: usize,
}

fn dir() -> PathBuf {
    let dir = crate::config::odei_home().join("calls");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Drop journals from sessions old enough that nobody is going to inspect them.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| now.duration_since(m).unwrap_or_default() > PRUNE_AFTER)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A session id is minted by us, but it names a file — keep it to a safe shape.
fn safe_id(session_id: &str) -> String {
    session_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

pub fn path_for(session_id: &str) -> PathBuf {
    dir().join(format!("{}.jsonl", safe_id(session_id)))
}

/// Every call in a session, oldest first. Reads from disk, so it works on a
/// resumed session too.
pub fn load(session_id: &str) -> Vec<Record> {
    let Ok(text) = std::fs::read_to_string(path_for(session_id)) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str::<Record>(line).ok()).collect()
}

impl Journal {
    pub fn new(session_id: &str) -> Journal {
        let dir = dir();
        prune(&dir);
        let path = path_for(session_id);
        // A resumed session continues its numbering.
        let next = std::fs::read_to_string(&path)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
            + 1;
        Journal { path, next }
    }

    /// Append a completed call and return the `#N` it was filed under.
    pub fn record(
        &mut self,
        tool: &str,
        label: &str,
        input: &Value,
        cwd: &Path,
        elapsed: Duration,
        outcome: &ToolOutcome,
    ) -> usize {
        let n = self.next;
        self.next += 1;
        let mut output = outcome.text.clone();
        if output.len() > OUTPUT_CAP {
            let cut = output
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| i >= output.len() - OUTPUT_CAP)
                .unwrap_or(0);
            output = format!("[earlier output dropped]\n{}", &output[cut..]);
        }
        let record = Record {
            n,
            tool: tool.to_string(),
            label: label.to_string(),
            input: input.clone(),
            cwd: cwd.display().to_string(),
            at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ms: elapsed.as_millis() as u64,
            is_error: outcome.is_error,
            output,
        };
        if let Ok(line) = serde_json::to_string(&record) {
            use std::io::Write as _;
            if let Ok(mut file) =
                std::fs::OpenOptions::new().create(true).append(true).open(&self.path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        n
    }
}

// ------------------------------------------------------------- reproduction

/// Wrap in single quotes for /bin/sh, escaping any single quotes inside.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/=:@+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn arg(input: &Value, key: &str) -> String {
    input[key].as_str().unwrap_or_default().to_string()
}

/// A shell command standing in for the call. `exact` marks the one case where
/// it is not a stand-in: the terminal tool really did run this.
pub struct Repro {
    pub exact: bool,
    pub lines: Vec<String>,
}

/// What the terminal tool ran, as a command line. Mirrors `build_command` in
/// tools::terminal — the env and shell flags there are the ones reproduced
/// here, so the two move together.
fn terminal_repro(input: &Value) -> Repro {
    let action = arg(input, "action");
    let command = arg(input, "command");
    if command.is_empty() {
        // A session-driving action: nothing was spawned, so nothing to rerun.
        let session = arg(input, "session_id");
        return Repro {
            exact: false,
            lines: vec![format!(
                "# terminal {action}{} — drives a live session, not a new command",
                if session.is_empty() { String::new() } else { format!(" on {session}") }
            )],
        };
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let captured = action == "exec";
    let mut line = String::new();
    if captured {
        // Pager and credential prompts are taken away from captured commands.
        line.push_str("PAGER=cat GIT_PAGER=cat GIT_TERMINAL_PROMPT=0 ");
    }
    if arg(input, "profile") == "clean" {
        let _ = write!(line, "/bin/sh -c {}", quote(&command));
    } else {
        let _ = write!(line, "{shell} -lc {}", quote(&command));
    }
    Repro { exact: true, lines: vec![line] }
}

/// The shell equivalent of a call, for the "run it yourself" block.
pub fn repro(record: &Record) -> Repro {
    let input = &record.input;
    let path = || quote(&arg(input, "path"));
    let approx = |line: String| Repro { exact: false, lines: vec![line] };
    match record.tool.as_str() {
        "terminal" => terminal_repro(input),
        "read_file" => {
            let start = input["start_line"].as_u64().unwrap_or(1).max(1);
            let count = input["line_count"].as_u64().unwrap_or(2000);
            approx(format!("sed -n '{start},{}p' {}", start + count - 1, path()))
        }
        "list_files" => {
            let p = arg(input, "path");
            approx(format!("ls -la {}", quote(if p.is_empty() { "." } else { &p })))
        }
        "file_info" => approx(format!("stat {}", path())),
        "grep_files" => {
            let mut line = String::from("grep -rn");
            if input["case_insensitive"].as_bool().unwrap_or(false) {
                line.push_str(" -i");
            }
            let include = arg(input, "include");
            if !include.is_empty() {
                let _ = write!(line, " --include={}", quote(&include));
            }
            let root = arg(input, "path");
            let _ = write!(
                line,
                " -- {} {}",
                quote(&arg(input, "pattern")),
                quote(if root.is_empty() { "." } else { &root })
            );
            approx(line)
        }
        "glob_files" => {
            let pattern = arg(input, "pattern");
            let root = arg(input, "path");
            let root = if root.is_empty() { ".".to_string() } else { root };
            // A pattern without a slash matches by filename at any depth;
            // one with a slash matches the whole relative path.
            let test = if pattern.contains('/') { "-path" } else { "-name" };
            let needle =
                if pattern.contains('/') { format!("*/{pattern}") } else { pattern.clone() };
            approx(format!("find {} -type f {test} {}", quote(&root), quote(&needle)))
        }
        "write_file" => {
            let content = arg(input, "content");
            let p = path();
            if content.len() > HEREDOC_CAP {
                approx(format!("# wrote {} bytes to {p} — body under Arguments", content.len()))
            } else {
                let mut lines = vec![format!("cat > {p} <<'ODEI'")];
                lines.extend(content.lines().map(str::to_string));
                lines.push("ODEI".into());
                Repro { exact: false, lines }
            }
        }
        "edit_file" => approx(format!("git diff -- {}  # the edit is under Arguments", path())),
        "delete_file" => approx(format!("rm {}", path())),
        "create_folder" => approx(format!("mkdir -p {}", path())),
        "rename_file" => approx(format!(
            "mv {} {}",
            quote(&arg(input, "old_path")),
            quote(&arg(input, "new_path"))
        )),
        "copy_file" => approx(format!(
            "cp {} {}",
            quote(&arg(input, "source")),
            quote(&arg(input, "destination"))
        )),
        "web_fetch" => approx(format!("curl -sL {}", quote(&arg(input, "url")))),
        "web_search" => approx(format!(
            "curl -s {}",
            quote(&format!(
                "https://html.duckduckgo.com/html/?q={}",
                arg(input, "query").replace(' ', "+")
            ))
        )),
        other => approx(format!("# {other} runs natively in odei — no shell equivalent")),
    }
}

// ----------------------------------------------------------------- report

fn rule(title: &str, width: usize) -> String {
    let head = format!("── {title} ");
    let dashes = width.saturating_sub(head.chars().count()).min(200);
    format!("{head}{}", "─".repeat(dashes))
}

/// The full account of one call, as plain text: header, the command to rerun,
/// the arguments, and the complete output. No colour — this gets paged,
/// selected, and pasted.
pub fn report(record: &Record, width: usize) -> String {
    // A report is a document, not a screenful: it is written once and then
    // read in a pane about half the width of the one that made it, so the
    // measure stays fixed rather than tracking the terminal.
    let width = width.clamp(40, 72);
    let status = if record.is_error { "failed" } else { "ok" };
    let seconds = record.ms as f64 / 1000.0;
    let mut out = format!(
        "odei call #{}  ·  {}  ·  {status}  ·  {seconds:.2}s  ·  {}\n{}\ncwd {}\n\n",
        record.n, record.tool, record.at, record.label, record.cwd
    );

    let repro = repro(record);
    let title = if repro.exact { "Ran this" } else { "Shell equivalent (approximate)" };
    let _ = writeln!(out, "{}", rule(title, width));
    if !repro.exact {
        let _ = writeln!(out, "# odei did this natively; the line below stands in for it");
    }
    let _ = writeln!(out, "cd {}", quote(&record.cwd));
    for line in &repro.lines {
        let _ = writeln!(out, "{line}");
    }

    let args = serde_json::to_string_pretty(&record.input).unwrap_or_default();
    if args.trim() != "{}" && !args.trim().is_empty() {
        let _ = write!(out, "\n{}\n{args}\n", rule("Arguments", width));
    }

    let bytes = record.output.len();
    let seen = if bytes > crate::tools::results::INLINE_CAP {
        format!(
            " · the model saw a {} KB preview of this",
            crate::tools::results::INLINE_CAP / 1024
        )
    } else {
        String::new()
    };
    let label = if record.output.is_empty() {
        "Output · empty".to_string()
    } else {
        format!("Output · {bytes} bytes · complete{seen}")
    };
    let _ = write!(out, "\n{}\n", rule(&label, width));
    if !record.output.is_empty() {
        let _ = writeln!(out, "{}", record.output.trim_end());
    }
    out
}

/// One line per call for the picker: `#7  ✗ terminal  Ran cargo test  2.4s`.
pub fn summary_line(record: &Record) -> String {
    let mark = if record.is_error { '✗' } else { ' ' };
    let seconds = record.ms as f64 / 1000.0;
    let timing = if seconds >= 0.05 { format!("{seconds:.1}s") } else { String::new() };
    let mut label = record.label.clone();
    if label.chars().count() > 52 {
        label = label.chars().take(51).collect::<String>() + "…";
    }
    format!("#{:<4}{mark} {:<16} {label:<53} {timing:>6}", record.n, record.tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(tool: &str, input: Value) -> Record {
        Record {
            n: 1,
            tool: tool.into(),
            label: "Did a thing".into(),
            input,
            cwd: "/tmp/proj".into(),
            at: "2026-08-20 18:22:07".into(),
            ms: 2400,
            is_error: false,
            output: "hello".into(),
        }
    }

    #[test]
    fn quoting_survives_hostile_arguments() {
        assert_eq!(quote("plain.txt"), "plain.txt");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("a; rm -rf /"), "'a; rm -rf /'");
    }

    #[test]
    fn terminal_repro_is_marked_exact_and_carries_the_env() {
        let r = repro(&record("terminal", json!({"action": "exec", "command": "cargo test"})));
        assert!(r.exact);
        let line = r.lines.join("\n");
        assert!(line.contains("PAGER=cat"), "{line}");
        assert!(line.contains("GIT_TERMINAL_PROMPT=0"), "{line}");
        assert!(line.contains("-lc 'cargo test'"), "{line}");

        // A clean profile skips the login shell.
        let r = repro(&record(
            "terminal",
            json!({"action": "exec", "command": "ls", "profile": "clean"}),
        ));
        assert!(r.lines[0].contains("/bin/sh -c ls"), "{:?}", r.lines);

        // Session actions spawn nothing, so they claim nothing.
        let r = repro(&record("terminal", json!({"action": "read", "session_id": "s1"})));
        assert!(!r.exact);
        assert!(r.lines[0].starts_with('#'), "{:?}", r.lines);
    }

    #[test]
    fn native_tools_get_approximate_shell_equivalents() {
        let r = repro(&record("read_file", json!({"path": "src/a.rs", "start_line": 10, "line_count": 5})));
        assert!(!r.exact);
        assert_eq!(r.lines[0], "sed -n '10,14p' src/a.rs");

        let r = repro(&record(
            "grep_files",
            json!({"pattern": "TODO x", "include": "*.rs", "case_insensitive": true}),
        ));
        assert_eq!(r.lines[0], "grep -rn -i --include='*.rs' -- 'TODO x' .");

        let r = repro(&record("glob_files", json!({"pattern": "*.rs"})));
        assert_eq!(r.lines[0], "find . -type f -name '*.rs'");

        let r = repro(&record("code_outline", json!({"path": "."})));
        assert!(r.lines[0].contains("no shell equivalent"), "{:?}", r.lines);
    }

    #[test]
    fn write_file_repro_is_a_heredoc_until_it_is_too_big() {
        let r = repro(&record("write_file", json!({"path": "a.txt", "content": "one\ntwo\n"})));
        assert_eq!(r.lines, vec!["cat > a.txt <<'ODEI'", "one", "two", "ODEI"]);

        let big = "x".repeat(HEREDOC_CAP + 1);
        let r = repro(&record("write_file", json!({"path": "a.txt", "content": big})));
        assert!(r.lines[0].contains("bytes to a.txt"), "{:?}", r.lines);
    }

    #[test]
    fn report_lays_out_header_repro_arguments_and_output() {
        let mut rec = record("terminal", json!({"action": "exec", "command": "cargo test"}));
        rec.is_error = true;
        rec.output = "test result: FAILED".into();
        let text = report(&rec, 72);
        assert!(text.starts_with("odei call #1"), "{text}");
        assert!(text.contains("failed"), "{text}");
        assert!(text.contains("cwd /tmp/proj"), "{text}");
        assert!(text.contains("── Ran this "), "{text}");
        assert!(text.contains("cd /tmp/proj"), "{text}");
        assert!(text.contains("── Arguments "), "{text}");
        assert!(text.contains("── Output · 19 bytes · complete"), "{text}");
        assert!(text.contains("test result: FAILED"), "{text}");
        // No escape codes: this gets paged and pasted.
        assert!(!text.contains('\x1b'), "report must stay plain text");
    }

    #[test]
    fn journal_numbers_calls_and_reloads_them() {
        let id = format!("test-journal-{}", std::process::id());
        let path = path_for(&id);
        let _ = std::fs::remove_file(&path);

        let mut journal = Journal::new(&id);
        let cwd = std::path::Path::new("/tmp/proj");
        assert_eq!(
            journal.record(
                "read_file",
                "Read a.rs",
                &json!({"path": "a.rs"}),
                cwd,
                Duration::from_millis(12),
                &ToolOutcome::ok("contents"),
            ),
            1
        );
        assert_eq!(
            journal.record(
                "terminal",
                "Ran ls",
                &json!({"action": "exec", "command": "ls"}),
                cwd,
                Duration::from_millis(30),
                &ToolOutcome::err("boom"),
            ),
            2
        );

        let loaded = load(&id);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].output, "contents");
        assert!(loaded[1].is_error);
        assert!(summary_line(&loaded[1]).contains("✗"));

        // A resumed session keeps counting instead of overwriting #1.
        let mut resumed = Journal::new(&id);
        assert_eq!(
            resumed.record(
                "file_info",
                "Inspected a.rs",
                &json!({"path": "a.rs"}),
                cwd,
                Duration::from_millis(1),
                &ToolOutcome::ok("file"),
            ),
            3
        );
        let _ = std::fs::remove_file(&path);
    }
}
