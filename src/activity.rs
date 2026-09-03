//! What a tool call looks like in the shell: the words after its label, the
//! stat when it finishes, and the block underneath when the user wants more.
//!
//! One `Call` view serves both the live activity line and `/expand N` over
//! the journal, so a call reads the same whether it just happened or is being
//! reopened an hour later.
//!
//! Every line here is built to a width and never wrapped. A tool call is
//! scanned, not read: if something does not fit, it is cut and counted.

use crate::agent::ToolDone;
use crate::calls::Record;
use crate::config::Detail;
use crate::diff::FileDiff;
use crate::theme::Theme;
use serde_json::Value;

/// Arguments this long are described rather than printed.
const ARG_INLINE_CAP: usize = 160;
/// Below this, a call is fast enough that its timing is noise.
const TIMING_FLOOR_MS: u64 = 200;

pub struct Call<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
    pub output: &'a str,
    pub is_error: bool,
    pub diff: Option<&'a FileDiff>,
    /// The journal's `#N`, for pointing at the rest of a long output.
    pub call: Option<usize>,
}

impl<'a> Call<'a> {
    pub fn of(done: &'a ToolDone<'a>) -> Call<'a> {
        Call {
            tool: done.tool,
            input: done.input,
            output: done.output,
            is_error: done.is_error,
            diff: done.diff,
            call: done.call,
        }
    }

    pub fn of_record(record: &'a Record) -> Call<'a> {
        Call {
            tool: &record.tool,
            input: &record.input,
            output: &record.output,
            is_error: record.is_error,
            diff: record.diff.as_ref(),
            call: Some(record.n),
        }
    }
}

// ---------------------------------------------------------------- qualifier

/// The words after the label: what the arguments say that "Read src/ui.rs"
/// does not. `None` when the label already tells the whole story.
pub fn qualifier(tool: &str, input: &Value) -> Option<String> {
    let text = |key: &str| input[key].as_str().filter(|v| !v.trim().is_empty());
    let number = |key: &str| input[key].as_u64();
    let mut parts: Vec<String> = Vec::new();
    match tool {
        "read_file" => {
            if let Some(start) = number("start_line").filter(|&n| n > 1) {
                match number("line_count") {
                    Some(count) => parts.push(format!("lines {start}–{}", start + count - 1)),
                    None => parts.push(format!("from line {start}")),
                }
            } else if let Some(count) = number("line_count") {
                parts.push(format!("first {count} lines"));
            }
        }
        "grep_files" | "glob_files" => {
            if let Some(include) = text("include") {
                parts.push(format!("in {include}"));
            }
            if let Some(root) = text("path").filter(|p| *p != ".") {
                parts.push(format!("under {root}"));
            }
            if input["case_insensitive"].as_bool().unwrap_or(false) {
                parts.push("any case".into());
            }
            match text("mode") {
                Some("count") => parts.push("counting".into()),
                Some("files_with_matches") => parts.push("file names only".into()),
                _ => {}
            }
            if let Some(context) = number("context_lines").filter(|&n| n > 0) {
                parts.push(format!("±{context} lines"));
            }
            if let Some(offset) = number("offset").filter(|&n| n > 0) {
                parts.push(format!("from result {offset}"));
            }
        }
        "terminal" => {
            let action = text("action").unwrap_or("exec");
            match action {
                "exec" => {}
                "start" => parts.push("as a session".into()),
                other => {
                    parts.push(other.to_string());
                    if let Some(id) = text("session_id") {
                        parts.push(id.to_string());
                    }
                }
            }
            if let Some(cwd) = text("cwd") {
                parts.push(format!("in {cwd}"));
            }
            if text("profile") == Some("clean") {
                parts.push("clean env".into());
            }
        }
        "rename_file" => {
            if let Some(new) = text("new_path") {
                parts.push(format!("→ {new}"));
            }
        }
        "copy_file" => {
            if let Some(destination) = text("destination") {
                parts.push(format!("→ {destination}"));
            }
        }
        "write_file" => {
            if let Some(content) = text("content") {
                parts.push(format!("{} lines", content.lines().count()));
            }
        }
        "read_tool_result" => {
            if let Some(query) = text("query") {
                parts.push(format!("looking for {query}"));
            } else if let Some(offset) = number("offset") {
                parts.push(format!("from byte {offset}"));
            }
        }
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

// --------------------------------------------------------------------- stat

impl Call<'_> {
    /// The right-hand end of a finished activity line: what the call
    /// produced, in as few words as still mean something.
    pub fn stat(&self) -> Option<String> {
        if self.is_error {
            // Why it failed belongs on the line itself — a failure is never
            // something the user should have to expand to understand.
            if let Some(code) = exit_code(self.output) {
                // A command's own last word explains it; its first is a
                // banner, and "(no output)" explains nothing at all.
                let reason = self.output.lines().map(str::trim).rev().find(|line| {
                    !line.is_empty() && !line.starts_with("exit code:") && *line != "(no output)"
                });
                return Some(match reason {
                    Some(line) => clip(&format!("exit {code} · {line}"), 72),
                    None => format!("exit {code}"),
                });
            }
            return first_line(self.output).map(|line| clip(line, 72));
        }
        if let Some(diff) = self.diff {
            if diff.is_empty() {
                return Some("no change".into());
            }
            let stat = diff.stat();
            return Some(if diff.created {
                format!("{stat} · new file")
            } else {
                stat
            });
        }
        let rows = || self.output.lines().filter(|l| !l.starts_with('[')).count();
        let plural = |n: usize, noun: &str| {
            // "match" pluralises the long way; every other noun here takes
            // a bare s.
            let suffix = if n == 1 {
                ""
            } else if noun.ends_with("ch") {
                "es"
            } else {
                "s"
            };
            format!("{n} {noun}{suffix}")
        };
        match self.tool {
            "read_file" => {
                let lines = self.output.lines().filter(|l| is_numbered(l)).count();
                Some(plural(lines, "line"))
            }
            "grep_files" => {
                if self.output.starts_with("no matches") {
                    return Some("no matches".into());
                }
                match self.input["mode"].as_str() {
                    Some("count") => first_line(self.output).map(|l| clip(l, 72)),
                    Some("files_with_matches") => Some(plural(rows(), "file")),
                    _ => Some(plural(
                        self.output
                            .lines()
                            .filter(|l| l.contains(':') && !l.starts_with('['))
                            .count(),
                        "match",
                    )),
                }
            }
            "glob_files" => match self.input["mode"].as_str() {
                Some("count") => first_line(self.output).map(|l| clip(l, 72)),
                _ => Some(plural(rows(), "file")),
            },
            "list_files" => {
                let entries = rows();
                Some(format!(
                    "{entries} entr{}",
                    if entries == 1 { "y" } else { "ies" }
                ))
            }
            "semantic_search" => Some(plural(rows(), "candidate")),
            "terminal" => {
                let mut parts = Vec::new();
                if let Some(code) = exit_code(self.output) {
                    parts.push(format!("exit {code}"));
                }
                let lines = self.output.lines().count();
                if lines > 1 {
                    parts.push(format!("{lines} lines"));
                }
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
            "web_fetch" => Some(bytes(self.output.len())),
            _ => {
                let lines = self.output.lines().count();
                (lines > 1).then(|| plural(lines, "line"))
            }
        }
    }

    // -------------------------------------------------------------- body

    /// The block under a finished activity line: the arguments worth naming,
    /// the diff, and as much output as `detail` allows. Lines come back
    /// without their margin — the caller draws that, so it can put its own
    /// tree bar there — but already cut to fit inside it.
    pub fn body(&self, theme: &Theme, detail: Detail, margin: usize, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        let dim = |text: String| format!("{}{text}{}", theme.dim, theme.reset());

        if detail.shows_arguments() {
            for (key, value) in arguments(self.tool, self.input, self.diff.is_some()) {
                out.push(dim(clip(
                    &format!("{key} {value}"),
                    width.saturating_sub(margin),
                )));
            }
        }

        if let Some(diff) = self
            .diff
            .filter(|d| !d.is_empty() && detail.diff_lines() > 0)
        {
            if diff.hunks.is_empty() {
                out.push(dim(format!("{} · too large to show", diff.stat())));
            } else {
                out.extend(crate::diff::render(
                    theme,
                    diff,
                    margin,
                    width,
                    detail.diff_lines(),
                ));
            }
        }

        let cap = if self.is_error {
            detail.error_lines()
        } else {
            detail.output_lines()
        };
        // A diff already says what a file-changing call did; its one-line
        // confirmation underneath would be noise.
        let redundant = self.diff.is_some() && !self.is_error;
        if cap > 0 && !redundant && !self.output.trim().is_empty() {
            let lines: Vec<&str> = self.output.lines().collect();
            // A command's reason for failing is at the end of its output; a
            // tool's is at the start of its message.
            let tail = self.tool == "terminal";
            let shown: Vec<&str> = if lines.len() <= cap {
                lines.clone()
            } else if tail {
                lines[lines.len() - cap..].to_vec()
            } else {
                lines[..cap].to_vec()
            };
            let elided = lines.len() - shown.len();
            if elided > 0 && tail {
                out.push(dim(format!(
                    "… {elided} earlier lines{}",
                    reference(self.call)
                )));
            }
            for line in shown {
                out.push(dim(clip(line, width.saturating_sub(margin))));
            }
            if elided > 0 && !tail {
                out.push(dim(format!(
                    "… {elided} more lines{}",
                    reference(self.call)
                )));
            }
        }
        out
    }
}

/// Where the rest of a truncated output lives.
fn reference(call: Option<usize>) -> String {
    match call {
        Some(n) => format!(" · /call {n}"),
        None => String::new(),
    }
}

/// The arguments of a call, as label/value pairs, skipping the one already in
/// the label and describing anything too long to print. `has_diff` drops the
/// text arguments of a file change: the diff below says the same thing, read
/// the way a person reads a change.
fn arguments(tool: &str, input: &Value, has_diff: bool) -> Vec<(String, String)> {
    let label_arg = crate::tools::find(tool)
        .map(|spec| spec.label_arg)
        .unwrap_or("");
    let Some(map) = input.as_object() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (key, value) in map {
        if has_diff && matches!(key.as_str(), "old_string" | "new_string" | "content") {
            continue;
        }
        let rendered = match value {
            Value::String(text) => {
                if text.chars().count() > ARG_INLINE_CAP || text.contains('\n') {
                    // The label_arg is the exception: a long shell command is
                    // the whole point of the call, so it is printed in full.
                    if key == label_arg {
                        text.lines().collect::<Vec<_>>().join(" ⏎ ")
                    } else {
                        format!("<{} bytes>", text.len())
                    }
                } else if key == label_arg {
                    // Already on the activity line.
                    continue;
                } else {
                    text.clone()
                }
            }
            Value::Null => continue,
            other => other.to_string(),
        };
        rows.push((key.clone(), rendered));
    }
    rows
}

// ------------------------------------------------------------------ helpers

/// `1.2s`, `12s`, `2m 04s` — or nothing at all when the call was quick.
pub fn duration(ms: u64) -> Option<String> {
    if ms < TIMING_FLOOR_MS {
        return None;
    }
    Some(if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000)
    })
}

pub fn bytes(count: usize) -> String {
    if count < 1024 {
        format!("{count} B")
    } else if count < 1024 * 1024 {
        format!("{:.1} KB", count as f64 / 1024.0)
    } else {
        format!("{:.1} MB", count as f64 / (1024.0 * 1024.0))
    }
}

/// A `k` suffix past a thousand, for token counts on the step line.
pub fn tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", count as f64 / 1000.0)
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The `exit code: N` the terminal tool appends to a captured run.
fn exit_code(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .take(2)
        .find_map(|line| line.trim().strip_prefix("exit code: "))
}

/// A line from `read_file`: right-aligned number, tab, source.
fn is_numbered(line: &str) -> bool {
    line.split_once('\t')
        .is_some_and(|(number, _)| number.trim().parse::<u64>().is_ok())
}

/// Make a line safe to print: tabs become spaces and control characters go,
/// so a tool result can never move the cursor or repaint the screen.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// Sanitize, then cut to `room`, marking that something was cut.
pub fn clip(text: &str, room: usize) -> String {
    let flat = sanitize(text);
    let flat = flat.trim_end();
    if flat.chars().count() <= room {
        flat.to_string()
    } else {
        flat.chars()
            .take(room.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call<'a>(tool: &'a str, input: &'a Value, output: &'a str) -> Call<'a> {
        Call {
            tool,
            input,
            output,
            is_error: false,
            diff: None,
            call: Some(7),
        }
    }

    #[test]
    fn qualifiers_say_what_the_label_cannot() {
        let input = json!({"path": "src/ui.rs", "start_line": 120, "line_count": 200});
        assert_eq!(qualifier("read_file", &input).unwrap(), "lines 120–319");

        let input =
            json!({"pattern": "TODO", "include": "*.rs", "path": "src", "case_insensitive": true});
        assert_eq!(
            qualifier("grep_files", &input).unwrap(),
            "in *.rs · under src · any case"
        );

        let input = json!({"action": "read", "session_id": "terminal-2"});
        assert_eq!(qualifier("terminal", &input).unwrap(), "read · terminal-2");

        // An ordinary exec adds nothing: the command is already the label.
        assert_eq!(
            qualifier("terminal", &json!({"action": "exec", "command": "ls"})),
            None
        );
        assert_eq!(qualifier("code_outline", &json!({"path": "."})), None);
    }

    #[test]
    fn stats_name_what_came_back() {
        let read = "     1\tfirst\n     2\tsecond\n[lines 1-2 of 900]";
        assert_eq!(
            call("read_file", &json!({"path": "a"}), read)
                .stat()
                .unwrap(),
            "2 lines"
        );

        let grep = "src/a.rs:3:TODO\nsrc/b.rs:9:TODO\n[showing 2 of 40 matching lines]";
        assert_eq!(
            call("grep_files", &json!({"pattern": "TODO"}), grep)
                .stat()
                .unwrap(),
            "2 matches"
        );
        assert_eq!(
            call("grep_files", &json!({"pattern": "x"}), "no matches")
                .stat()
                .unwrap(),
            "no matches"
        );

        let run = "compiling\nwarning: unused\nexit code: 0";
        assert_eq!(
            call("terminal", &json!({"action": "exec"}), run)
                .stat()
                .unwrap(),
            "exit 0 · 3 lines"
        );

        // One line that only restates the label earns no stat.
        assert_eq!(
            call("delete_file", &json!({"path": "a"}), "Deleted a").stat(),
            None
        );
    }

    #[test]
    fn a_failure_puts_its_reason_on_the_line() {
        let input = json!({"path": "a"});
        let mut failed = call("edit_file", &input, "old_string not found in a");
        failed.is_error = true;
        assert_eq!(failed.stat().unwrap(), "old_string not found in a");

        // A command is explained by its exit code and its last word, not by
        // the banner it opened with.
        let exec = json!({"action": "exec"});
        let mut ran = call(
            "terminal",
            &exec,
            "running 2 tests\ntest result: FAILED. 1 passed; 1 failed\nexit code: 101",
        );
        ran.is_error = true;
        assert_eq!(
            ran.stat().unwrap(),
            "exit 101 · test result: FAILED. 1 passed; 1 failed"
        );

        // A command that said nothing at all still reports its code.
        let mut silent = call("terminal", &exec, "(no output)\nexit code: 7");
        silent.is_error = true;
        assert_eq!(silent.stat().unwrap(), "exit 7");
    }

    #[test]
    fn a_diff_is_the_stat_when_there_is_one() {
        let diff = crate::diff::compute("src/a.rs", "a\nb\n", "a\nB\n", false);
        let edit_input = json!({"path": "src/a.rs"});
        let mut edited = call("edit_file", &edit_input, "Edited src/a.rs");
        edited.diff = Some(&diff);
        assert_eq!(edited.stat().unwrap(), "+1 −1");

        let created = crate::diff::compute("new.rs", "", "one\n", true);
        let write_input = json!({"path": "new.rs"});
        let mut wrote = call("write_file", &write_input, "Created new.rs");
        wrote.diff = Some(&created);
        assert_eq!(wrote.stat().unwrap(), "+1 · new file");

        // A write that changed nothing says so rather than showing +0 −0.
        let same = crate::diff::compute("a.rs", "x\n", "x\n", false);
        wrote.diff = Some(&same);
        assert_eq!(wrote.stat().unwrap(), "no change");
    }

    #[test]
    fn the_body_shows_the_diff_and_holds_the_output_back() {
        let theme = crate::theme::plain();
        let diff = crate::diff::compute("src/a.rs", "a\nb\n", "a\nB\n", false);
        let input = json!({"path": "src/a.rs", "old_string": "b", "new_string": "B"});
        let mut edited = call("edit_file", &input, "Edited src/a.rs at line 2");
        edited.diff = Some(&diff);

        let normal = edited.body(theme, Detail::Normal, 4, 80);
        assert_eq!(normal.len(), 3, "{normal:?}");
        assert!(normal[1].contains("- b"), "{normal:?}");
        assert!(normal[2].contains("+ B"), "{normal:?}");
        // The tool's own "Edited …" line adds nothing next to the diff.
        assert!(!normal.iter().any(|l| l.contains("Edited")), "{normal:?}");

        // Collapsed shows nothing at all.
        assert!(edited.body(theme, Detail::Collapsed, 4, 80).is_empty());

        // Expanded adds the arguments — but not the text of the change,
        // which is what the diff already is.
        let expanded = edited.body(theme, Detail::Expanded, 4, 80);
        assert!(
            !expanded.iter().any(|l| l.contains("old_string")),
            "{expanded:?}"
        );
        assert!(
            !expanded.iter().any(|l| l.contains("new_string")),
            "{expanded:?}"
        );
    }

    #[test]
    fn a_failure_shows_its_output_even_when_folded_to_normal() {
        let theme = crate::theme::plain();
        let input = json!({"action": "exec"});
        let mut failed = call("terminal", &input, "line one\nboom\nexit code: 1");
        failed.is_error = true;
        let body = failed.body(theme, Detail::Normal, 4, 80);
        assert!(body.iter().any(|l| l.contains("boom")), "{body:?}");
        assert!(body.iter().any(|l| l.contains("exit code: 1")), "{body:?}");
    }

    #[test]
    fn long_output_is_cut_from_the_end_that_matters() {
        let theme = crate::theme::plain();
        let long: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        // A command's tail is the interesting end.
        let exec = json!({"action": "exec"});
        let mut ran = call("terminal", &exec, &long);
        ran.is_error = true;
        let body = ran.body(theme, Detail::Normal, 2, 80);
        assert!(
            body[0].contains("… 34 earlier lines · /call 7"),
            "{:?}",
            body[0]
        );
        assert!(
            body.last().unwrap().contains("line 40"),
            "{:?}",
            body.last()
        );

        // A read's head is.
        let path = json!({"path": "a"});
        let read = call("read_file", &path, &long);
        let body = read.body(theme, Detail::Expanded, 2, 80);
        assert!(body[0].contains("line 1"), "{:?}", body[0]);
        assert!(
            body.last().unwrap().contains("… 20 more lines · /call 7"),
            "{:?}",
            body.last()
        );
    }

    #[test]
    fn arguments_describe_what_they_cannot_print() {
        let big = "x".repeat(400);
        let input = json!({"path": "a.rs", "content": big});
        let rows = arguments("write_file", &input, false);
        assert!(
            rows.iter()
                .any(|(k, v)| k == "content" && v == "<400 bytes>"),
            "{rows:?}"
        );
        // The label argument is not repeated under the label.
        assert!(!rows.iter().any(|(k, _)| k == "path"), "{rows:?}");

        // Except when it is a multi-line command, which the label truncated.
        let input = json!({"action": "exec", "command": "set -e\nmake test"});
        let rows = arguments("terminal", &input, false);
        assert!(
            rows.iter()
                .any(|(k, v)| k == "command" && v == "set -e ⏎ make test"),
            "{rows:?}"
        );
    }

    #[test]
    fn timings_below_the_floor_are_not_worth_a_line() {
        assert_eq!(duration(12), None);
        assert_eq!(duration(1200).unwrap(), "1.2s");
        assert_eq!(duration(12_000).unwrap(), "12s");
        assert_eq!(duration(124_000).unwrap(), "2m 04s");
    }

    #[test]
    fn clip_strips_control_codes_so_output_cannot_repaint_the_screen() {
        assert_eq!(clip("\x1b[2Jgone", 40), "[2Jgone");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(tokens(12_400), "12.4k");
    }
}
