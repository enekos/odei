//! Off-transcript storage for large tool results.
//!
//! A tool that returns more than the inline budget gets its full output
//! written to `~/.odei/tool-results/` and only a head/tail preview placed in
//! the conversation, tagged with a handle. `read_tool_result` reads the rest
//! by byte range or searches it, so a 2 MB build log costs a few hundred
//! tokens of context until the model actually needs a specific part of it.

use super::{ToolContext, ToolOutcome};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Above this, a result is stored and previewed rather than inlined whole.
pub const INLINE_CAP: usize = 8 * 1024;
const PREVIEW_HEAD: usize = 6 * 1024;
const PREVIEW_TAIL: usize = 2 * 1024;
const READ_DEFAULT: usize = 8 * 1024;
const READ_MAX: usize = 32 * 1024;
const PRUNE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Process-wide, so two `Store` instances in one process cannot mint the same
/// handle. The pid in the handle covers concurrent processes.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    dir: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

fn char_boundary_at_or_before(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn char_boundary_at_or_after(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

impl Store {
    pub fn new() -> Store {
        let dir = crate::config::odei_home().join("tool-results");
        let _ = std::fs::create_dir_all(&dir);
        let store = Store { dir };
        store.prune();
        store
    }

    /// Drop results from earlier sessions that nothing can reference anymore.
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
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

    fn valid_handle(handle: &str) -> bool {
        handle.len() < 64
            && handle.starts_with("tr-")
            && handle[3..].chars().all(|c| c.is_ascii_digit() || c == '-')
    }

    fn path(&self, handle: &str) -> Option<PathBuf> {
        Self::valid_handle(handle).then(|| self.dir.join(format!("{handle}.txt")))
    }

    /// Persist `text` and return its handle.
    pub fn put(&self, text: &str) -> std::io::Result<String> {
        let stamp = chrono::Local::now().timestamp().max(0) as u64;
        let n = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let handle = format!("tr-{stamp}-{}-{n}", std::process::id());
        let path = self
            .path(&handle)
            .ok_or_else(|| std::io::Error::other("bad handle"))?;
        std::fs::write(&path, text)?;
        Ok(handle)
    }

    pub fn get(&self, handle: &str) -> std::io::Result<String> {
        let path = self
            .path(handle)
            .ok_or_else(|| std::io::Error::other(format!("malformed handle {handle}")))?;
        std::fs::read_to_string(path)
    }

    /// Build what actually goes into the conversation for an oversized result:
    /// the head, the tail, and instructions for reaching the middle.
    pub fn preview(&self, text: &str) -> String {
        let total = text.len();
        let head_end = char_boundary_at_or_before(text, PREVIEW_HEAD);
        let tail_start = char_boundary_at_or_after(text, total.saturating_sub(PREVIEW_TAIL));

        match self.put(text) {
            Ok(handle) => {
                let hidden = tail_start.saturating_sub(head_end);
                format!(
                    "{}\n\n[… {hidden} bytes omitted of {total} total. Full result saved as {handle} — \
                     read_tool_result with handle=\"{handle}\" takes an offset/length or a query to \
                     search it. Resume from offset {head_end}. …]\n\n{}",
                    &text[..head_end],
                    &text[tail_start..]
                )
            }
            Err(_) => {
                // Storage failed; degrade to a plain truncation rather than
                // handing back a handle that cannot be read.
                format!(
                    "{}\n\n[truncated: {total} bytes total, and this result could not be saved for paging]",
                    &text[..head_end]
                )
            }
        }
    }
}

pub fn read_tool_result(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(handle) = input["handle"].as_str() else {
        return ToolOutcome::err("missing required field: handle");
    };
    let text = match ctx.results.get(handle) {
        Ok(text) => text,
        Err(e) => {
            return ToolOutcome::err(format!(
                "cannot read {handle}: {e}. Handles come from oversized tool results in this session and expire after 7 days."
            ))
        }
    };
    let total = text.len();

    if let Some(query) = input["query"].as_str() {
        if query.is_empty() {
            return ToolOutcome::err("query must not be empty");
        }
        const WINDOW: usize = 400;
        const MAX_HITS: usize = 12;
        let mut out = String::new();
        let mut hits = 0;
        let mut from = 0;
        while let Some(found) = text[from..].find(query) {
            let at = from + found;
            let lo = char_boundary_at_or_before(&text, at.saturating_sub(WINDOW / 2));
            let hi = char_boundary_at_or_after(&text, (at + query.len() + WINDOW / 2).min(total));
            let _ = write!(out, "--- offset {at} ---\n{}\n", &text[lo..hi]);
            hits += 1;
            if hits >= MAX_HITS {
                break;
            }
            from = at + query.len();
        }
        return if hits == 0 {
            ToolOutcome::ok(format!(
                "{query:?} does not appear in {handle} ({total} bytes)"
            ))
        } else {
            ToolOutcome::ok(format!(
                "{hits} match{} for {query:?} in {handle} ({total} bytes):\n\n{out}",
                if hits == 1 { "" } else { "es" }
            ))
        };
    }

    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    if offset >= total && total > 0 {
        return ToolOutcome::err(format!(
            "offset {offset} is past the end of {handle} ({total} bytes)"
        ));
    }
    let length = input["length"]
        .as_u64()
        .map(|v| (v as usize).min(READ_MAX))
        .unwrap_or(READ_DEFAULT);
    let start = char_boundary_at_or_after(&text, offset);
    let end = char_boundary_at_or_before(&text, (start + length).min(total));
    let mut out = text[start..end].to_string();
    if end < total {
        let _ = write!(
            out,
            "\n\n[bytes {start}–{end} of {total}; continue with offset={end}]"
        );
    } else {
        let _ = write!(out, "\n\n[bytes {start}–{end} of {total}; end of result]");
    }
    ToolOutcome::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_keeps_head_and_tail_and_stores_the_whole_thing() {
        let store = Store::new();
        let body = format!("HEAD{}TAIL", "x".repeat(40_000));
        let preview = store.preview(&body);
        assert!(preview.starts_with("HEAD"));
        assert!(preview.trim_end().ends_with("TAIL"));
        assert!(preview.len() < body.len());
        let handle = preview
            .split("saved as ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .expect("handle in preview");
        assert_eq!(store.get(handle).unwrap(), body);
    }

    #[test]
    fn ranges_page_through_a_stored_result() {
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let body: String = (1..=4000).map(|n| format!("line {n}\n")).collect();
        let handle = ctx.results.put(&body).unwrap();

        let head = read_tool_result(&ctx, &json!({"handle": handle, "length": 64}));
        assert!(!head.is_error, "{}", head.text);
        assert!(head.text.starts_with("line 1\n"), "{}", head.text);
        assert!(
            head.text.contains("continue with offset=64"),
            "{}",
            head.text
        );

        let mid = read_tool_result(&ctx, &json!({"handle": handle, "offset": 64, "length": 32}));
        assert!(!mid.is_error, "{}", mid.text);
        assert!(!mid.text.starts_with("line 1\n"));

        let past = read_tool_result(&ctx, &json!({"handle": handle, "offset": 10_000_000}));
        assert!(past.is_error, "reading past the end should say so");
    }

    #[test]
    fn queries_return_windows_with_offsets() {
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let body = format!(
            "{}\nerror: it broke\n{}",
            "noise\n".repeat(2000),
            "more\n".repeat(2000)
        );
        let handle = ctx.results.put(&body).unwrap();

        let hit = read_tool_result(&ctx, &json!({"handle": handle, "query": "error:"}));
        assert!(!hit.is_error, "{}", hit.text);
        assert!(hit.text.contains("error: it broke"), "{}", hit.text);
        assert!(
            hit.text.contains("--- offset "),
            "windows are labelled with offsets"
        );

        let miss = read_tool_result(&ctx, &json!({"handle": handle, "query": "not-in-there"}));
        assert!(!miss.is_error);
        assert!(miss.text.contains("does not appear"), "{}", miss.text);
    }

    #[test]
    fn handles_are_validated_against_traversal() {
        let store = Store::new();
        assert!(store.get("../../etc/passwd").is_err());
        assert!(store.get("tr-1-1/../../etc/passwd").is_err());
    }
}
