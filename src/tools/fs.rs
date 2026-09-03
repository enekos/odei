//! Filesystem tools: bounded line-numbered reads, exact-once edit
//! matching, one-level listing, literal-substring grep.

use super::{ToolContext, ToolOutcome};
use globset::{Glob, GlobMatcher};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;
use walkdir::WalkDir;

const READ_LINE_CAP: usize = 2000;
const READ_BYTE_CAP: usize = 256 * 1024;
const LIST_ENTRY_CAP: usize = 500;
const GREP_RESULT_CAP: usize = 200;
const GREP_FILE_BYTE_CAP: u64 = 4 * 1024 * 1024;
const GLOB_MATCH_CAP: usize = 400;
const CONTEXT_LINES_CAP: usize = 10;

pub(crate) fn display_rel(ctx: &ToolContext, path: &Path) -> String {
    path.strip_prefix(&ctx.workspace_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub(crate) fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "zig-out"
            | "zig-cache"
            | ".zig-cache"
            | "dist"
            | ".next"
            | ".venv"
            | "__pycache__"
            | ".DS_Store"
    )
}

pub fn read_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(path) = input["path"].as_str() else {
        return ToolOutcome::err("missing required field: path");
    };
    let resolved = ctx.resolve(path);
    let text = match std::fs::read(&resolved) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return ToolOutcome::err(format!("{path} is not valid UTF-8 text")),
        },
        Err(e) => return ToolOutcome::err(format!("cannot read {path}: {e}")),
    };

    let start_line = input["start_line"]
        .as_u64()
        .map(|v| v.max(1) as usize)
        .unwrap_or(1);
    let line_count = input["line_count"]
        .as_u64()
        .map(|v| (v as usize).clamp(1, READ_LINE_CAP))
        .unwrap_or(READ_LINE_CAP);

    let total_lines = text.lines().count();
    let mut out = String::new();
    let mut bytes = 0usize;
    let mut emitted = 0usize;
    let mut truncated_by_bytes = false;
    for (i, line) in text
        .lines()
        .enumerate()
        .skip(start_line - 1)
        .take(line_count)
    {
        let numbered = format!("{:>6}\t{}\n", i + 1, line);
        if bytes + numbered.len() > READ_BYTE_CAP {
            truncated_by_bytes = true;
            break;
        }
        bytes += numbered.len();
        out.push_str(&numbered);
        emitted += 1;
    }
    if emitted == 0 && start_line > total_lines {
        return ToolOutcome::err(format!(
            "start_line {start_line} is past the end of {path} ({total_lines} lines)"
        ));
    }
    let last = start_line + emitted - 1;
    if truncated_by_bytes || last < total_lines && emitted == line_count {
        let _ = write!(
            out,
            "[showing lines {start_line}-{last} of {total_lines}; continue with start_line={}]",
            last + 1
        );
        // A structural map of what was cut off, so the next read can jump
        // straight to the right start_line instead of paging.
        if let Some(lang) = super::outline::Lang::of(&resolved) {
            let items = super::outline::outline(lang, &text);
            let rest: Vec<_> = items.into_iter().filter(|i| i.line > last).collect();
            if !rest.is_empty() {
                let _ = write!(
                    out,
                    "\n[declarations in the unread part — jump with start_line:\n{}]",
                    super::outline::render(&rest, 40, 3072).trim_end()
                );
            }
        }
    } else if start_line > 1 || last < total_lines {
        let _ = write!(out, "[lines {start_line}-{last} of {total_lines}]");
    }
    ToolOutcome::ok(out)
}

pub fn write_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let (Some(path), Some(content)) = (input["path"].as_str(), input["content"].as_str()) else {
        return ToolOutcome::err("missing required fields: path, content");
    };
    let resolved = ctx.resolve(path);
    if let Some(parent) = resolved.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::err(format!("cannot create parent directory: {e}"));
            }
        }
    }
    let existed = resolved.exists();
    // The only moment both sides of the change exist. Non-UTF-8 previous
    // contents simply produce no diff rather than failing the write.
    let previous = if existed {
        std::fs::read_to_string(&resolved).unwrap_or_default()
    } else {
        String::new()
    };
    match std::fs::write(&resolved, content) {
        Ok(()) => {
            let rel = display_rel(ctx, &resolved);
            let diff = crate::diff::compute(&rel, &previous, content, !existed);
            ToolOutcome::ok(format!(
                "{} {rel} ({} bytes)",
                if existed { "Overwrote" } else { "Created" },
                content.len()
            ))
            .with_diff(diff)
        }
        Err(e) => ToolOutcome::err(format!("cannot write {path}: {e}")),
    }
}

pub fn edit_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let (Some(path), Some(old), Some(new)) = (
        input["path"].as_str(),
        input["old_string"].as_str(),
        input["new_string"].as_str(),
    ) else {
        return ToolOutcome::err("missing required fields: path, old_string, new_string");
    };
    if old == new {
        return ToolOutcome::err("old_string and new_string are identical");
    }
    let resolved = ctx.resolve(path);
    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::err(format!("cannot read {path}: {e}")),
    };
    let occurrences = text.matches(old).count();
    if occurrences == 0 {
        return ToolOutcome::err(format!("old_string not found in {path}"));
    }
    if occurrences > 1 {
        return ToolOutcome::err(format!(
            "old_string matches {occurrences} times in {path}; it must match exactly once — extend it with surrounding context"
        ));
    }
    let updated = text.replacen(old, new, 1);
    match std::fs::write(&resolved, &updated) {
        Ok(()) => {
            // Newlines before the match, plus one: `lines().count()` would
            // report the line above whenever the match starts a line, and the
            // model notices the file disagreeing with the tool.
            let line = text[..text.find(old).unwrap()].matches('\n').count() + 1;
            let rel = display_rel(ctx, &resolved);
            // Diffed whole-file rather than old_string against new_string, so
            // the hunk carries the real line numbers and its true context.
            let diff = crate::diff::compute(&rel, &text, &updated, false);
            ToolOutcome::ok(format!("Edited {rel} at line {line}")).with_diff(diff)
        }
        Err(e) => ToolOutcome::err(format!("cannot write {path}: {e}")),
    }
}

pub fn list_files(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let path = input["path"].as_str().unwrap_or(".");
    let resolved = ctx.resolve(path);
    let entries = match std::fs::read_dir(&resolved) {
        Ok(entries) => entries,
        Err(e) => return ToolOutcome::err(format!("cannot list {path}: {e}")),
    };
    let mut rows: Vec<(bool, String, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.map(|m| m.len()).unwrap_or(0);
        rows.push((is_dir, name, size));
    }
    rows.sort_by_key(|a| (!a.0, a.1.to_lowercase()));
    let total = rows.len();
    let mut out = String::new();
    for (is_dir, name, size) in rows.into_iter().take(LIST_ENTRY_CAP) {
        if is_dir {
            let _ = writeln!(out, "{name}/");
        } else {
            let _ = writeln!(out, "{name} ({size} bytes)");
        }
    }
    if total > LIST_ENTRY_CAP {
        let _ = write!(out, "[{LIST_ENTRY_CAP} of {total} entries shown]");
    } else if total == 0 {
        out.push_str("(empty directory)");
    }
    ToolOutcome::ok(out)
}

fn build_matcher(pattern: &str) -> Result<GlobMatcher, String> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("invalid glob pattern: {e}"))
}

pub fn glob_files(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(pattern) = input["pattern"].as_str() else {
        return ToolOutcome::err("missing required field: pattern");
    };
    let root = ctx.resolve(input["path"].as_str().unwrap_or("."));
    let count_only = input["mode"].as_str() == Some("count");
    let matcher = match build_matcher(pattern) {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(e),
    };
    // A bare-name pattern like *.md should match at any depth relative root.
    let bare = !pattern.contains('/');
    let mut matches: Vec<String> = Vec::new();
    let mut count = 0usize;
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && skip_dir(&e.file_name().to_string_lossy())))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let hit = if bare {
            matcher.is_match(Path::new(entry.file_name()))
        } else {
            matcher.is_match(rel)
        };
        if hit {
            count += 1;
            if !count_only && matches.len() < GLOB_MATCH_CAP {
                matches.push(rel.display().to_string());
            }
        }
    }
    if count_only {
        return ToolOutcome::ok(format!("{count} matching paths"));
    }
    if matches.is_empty() {
        return ToolOutcome::ok(format!("no files match {pattern}"));
    }
    matches.sort();
    let mut out = matches.join("\n");
    if count > GLOB_MATCH_CAP {
        let _ = write!(
            out,
            "\n[{GLOB_MATCH_CAP} of {count} matches shown; narrow the pattern]"
        );
    }
    ToolOutcome::ok(out)
}

pub fn grep_files(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(pattern) = input["pattern"].as_str() else {
        return ToolOutcome::err("missing required field: pattern");
    };
    if pattern.is_empty() {
        return ToolOutcome::err("pattern must not be empty");
    }
    let root = ctx.resolve(input["path"].as_str().unwrap_or("."));
    let case_insensitive = input["case_insensitive"].as_bool().unwrap_or(false);
    let mode = input["mode"].as_str().unwrap_or("matches");
    let head_limit = input["head_limit"]
        .as_u64()
        .map(|v| (v as usize).clamp(1, GREP_RESULT_CAP))
        .unwrap_or(GREP_RESULT_CAP);
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    let context_lines = input["context_lines"]
        .as_u64()
        .map(|v| (v as usize).min(CONTEXT_LINES_CAP))
        .unwrap_or(0);
    let include = match input["include"].as_str() {
        Some(pattern) => match build_matcher(pattern) {
            Ok(m) => Some((m, !pattern.contains('/'))),
            Err(e) => return ToolOutcome::err(e),
        },
        None => None,
    };

    let needle = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let mut line_hits = 0usize;
    let mut file_hits: Vec<String> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    'files: for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && skip_dir(&e.file_name().to_string_lossy())))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.len() > GREP_FILE_BYTE_CAP)
            .unwrap_or(true)
        {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if let Some((matcher, bare)) = &include {
            let hit = if *bare {
                matcher.is_match(Path::new(&entry.file_name().to_string_lossy().to_string()))
            } else {
                matcher.is_match(rel)
            };
            if !hit {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let mut file_matched = false;
        // Computed on the first hit in a supported source file; annotates
        // each match with the declaration it sits in.
        let mut file_outline: Option<Vec<super::outline::Item>> = None;
        for (i, line) in lines.iter().enumerate() {
            let hay = if case_insensitive {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            if hay.contains(&needle) {
                line_hits += 1;
                if !file_matched {
                    file_matched = true;
                    file_hits.push(rel.display().to_string());
                }
                if mode == "matches" {
                    if skipped < offset {
                        skipped += 1;
                    } else if rows.len() < head_limit {
                        if context_lines > 0 {
                            let lo = i.saturating_sub(context_lines);
                            let hi = (i + context_lines).min(lines.len() - 1);
                            for (j, ctx_line) in lines[lo..=hi].iter().enumerate() {
                                let n = lo + j;
                                let sep = if n == i { ':' } else { '-' };
                                rows.push(format!(
                                    "{}{sep}{}{sep}{}",
                                    rel.display(),
                                    n + 1,
                                    ctx_line
                                ));
                            }
                            rows.push("--".into());
                        } else {
                            if file_outline.is_none() {
                                file_outline = Some(
                                    super::outline::Lang::of(entry.path())
                                        .map(|l| super::outline::outline(l, &text))
                                        .unwrap_or_default(),
                                );
                            }
                            let mut row = format!("{}:{}:{}", rel.display(), i + 1, line);
                            if let Some(enc) = file_outline
                                .as_deref()
                                .and_then(|items| super::outline::enclosing(items, i + 1))
                            {
                                if enc.line != i + 1 {
                                    let _ = write!(
                                        row,
                                        "  [in {}]",
                                        super::outline::short_name(&enc.text)
                                    );
                                }
                            }
                            rows.push(row);
                        }
                    } else if mode == "matches" && context_lines == 0 {
                        // keep counting totals; rows already full
                    }
                }
            }
        }
        if mode == "files_with_matches" && file_hits.len() >= offset + head_limit {
            break 'files;
        }
    }

    match mode {
        "count" => ToolOutcome::ok(format!(
            "{line_hits} matching lines in {} matching files",
            file_hits.len()
        )),
        "files_with_matches" => {
            let page: Vec<String> = file_hits
                .into_iter()
                .skip(offset)
                .take(head_limit)
                .collect();
            if page.is_empty() {
                ToolOutcome::ok("no matches".to_string())
            } else {
                ToolOutcome::ok(page.join("\n"))
            }
        }
        _ => {
            if rows.is_empty() {
                ToolOutcome::ok("no matches".to_string())
            } else {
                let mut out = rows.join("\n");
                if line_hits > offset + head_limit && context_lines == 0 {
                    let _ = write!(
                        out,
                        "\n[showing {} of {line_hits} matching lines; continue with offset={}]",
                        head_limit,
                        offset + head_limit
                    );
                }
                ToolOutcome::ok(out)
            }
        }
    }
}

pub fn delete_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(path) = input["path"].as_str() else {
        return ToolOutcome::err("missing required field: path");
    };
    let resolved = ctx.resolve(path);
    let meta = match std::fs::symlink_metadata(&resolved) {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(format!("cannot inspect {path}: {e}")),
    };
    let result = if meta.is_dir() {
        std::fs::remove_dir(&resolved).map_err(|e| {
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                format!("{path} is not empty; delete_file only removes files or empty directories")
            } else {
                format!("cannot delete {path}: {e}")
            }
        })
    } else {
        std::fs::remove_file(&resolved).map_err(|e| format!("cannot delete {path}: {e}"))
    };
    match result {
        Ok(()) => ToolOutcome::ok(format!("Deleted {}", display_rel(ctx, &resolved))),
        Err(msg) => ToolOutcome::err(msg),
    }
}

pub fn rename_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let (Some(old), Some(new)) = (input["old_path"].as_str(), input["new_path"].as_str()) else {
        return ToolOutcome::err("missing required fields: old_path, new_path");
    };
    let from = ctx.resolve(old);
    let to = ctx.resolve(new);
    if to.exists() {
        return ToolOutcome::err(format!("{new} already exists; refusing to overwrite"));
    }
    if let Some(parent) = to.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => ToolOutcome::ok(format!(
            "Renamed {} -> {}",
            display_rel(ctx, &from),
            display_rel(ctx, &to)
        )),
        Err(e) => ToolOutcome::err(format!("cannot rename {old}: {e}")),
    }
}

pub fn copy_file(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let (Some(source), Some(dest)) = (input["source"].as_str(), input["destination"].as_str())
    else {
        return ToolOutcome::err("missing required fields: source, destination");
    };
    let from = ctx.resolve(source);
    let to = ctx.resolve(dest);
    if to.exists() {
        return ToolOutcome::err(format!("{dest} already exists; refusing to overwrite"));
    }
    if let Some(parent) = to.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::copy(&from, &to) {
        Ok(bytes) => ToolOutcome::ok(format!(
            "Copied {} -> {} ({bytes} bytes)",
            display_rel(ctx, &from),
            display_rel(ctx, &to)
        )),
        Err(e) => ToolOutcome::err(format!("cannot copy {source}: {e}")),
    }
}

pub fn create_folder(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(path) = input["path"].as_str() else {
        return ToolOutcome::err("missing required field: path");
    };
    let resolved = ctx.resolve(path);
    if resolved.exists() {
        return ToolOutcome::err(format!("{path} already exists"));
    }
    match std::fs::create_dir_all(&resolved) {
        Ok(()) => ToolOutcome::ok(format!("Created {}/", display_rel(ctx, &resolved))),
        Err(e) => ToolOutcome::err(format!("cannot create {path}: {e}")),
    }
}

pub fn file_info(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(path) = input["path"].as_str() else {
        return ToolOutcome::err("missing required field: path");
    };
    let resolved = ctx.resolve(path);
    let meta = match std::fs::symlink_metadata(&resolved) {
        Ok(m) => m,
        Err(_) => return ToolOutcome::ok(format!("{path}: does not exist")),
    };
    let kind = if meta.is_dir() {
        "directory"
    } else if meta.file_type().is_symlink() {
        "symlink"
    } else {
        "file"
    };
    let modified = meta
        .modified()
        .ok()
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into());
    ToolOutcome::ok(format!(
        "{}: {kind}, {} bytes, modified {modified}",
        display_rel(ctx, &resolved),
        meta.len()
    ))
}

/// Keyword ranking, not vector similarity: split the query into words and
/// score each file by how many distinct words it contains, then by total hits.
pub fn semantic_search(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(query) = input["query"].as_str() else {
        return ToolOutcome::err("missing required field: query");
    };
    let keywords: Vec<String> = query
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| w.len() >= 3)
        .collect();
    if keywords.is_empty() {
        return ToolOutcome::err("query has no usable keywords");
    }
    let mut scored: Vec<(usize, usize, String)> = Vec::new();
    for entry in WalkDir::new(&ctx.workspace_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && skip_dir(&e.file_name().to_string_lossy())))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.len() > GREP_FILE_BYTE_CAP)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let hay = text.to_lowercase();
        let mut distinct = 0usize;
        let mut total = 0usize;
        for keyword in &keywords {
            let hits = hay.matches(keyword.as_str()).count();
            if hits > 0 {
                distinct += 1;
                total += hits;
            }
        }
        if distinct > 0 {
            let rel = entry
                .path()
                .strip_prefix(&ctx.workspace_root)
                .unwrap_or(entry.path())
                .display()
                .to_string();
            scored.push((distinct, total, rel));
        }
    }
    scored.sort_by_key(|s| std::cmp::Reverse((s.0, s.1)));
    if scored.is_empty() {
        return ToolOutcome::ok("no files match the concept keywords".to_string());
    }
    let mut out = String::new();
    for (distinct, total, rel) in scored.into_iter().take(20) {
        let _ = writeln!(
            out,
            "{rel} ({distinct}/{} keywords, {total} hits)",
            keywords.len()
        );
    }
    ToolOutcome::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir)
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("odei-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_file_numbers_lines() {
        let dir = tempdir("read");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = read_file(&ctx(&dir), &json!({"path": "a.txt"}));
        assert!(!out.is_error);
        assert!(out.text.contains("     1\tone"));
        assert!(out.text.contains("     3\tthree"));
    }

    #[test]
    fn edit_file_requires_unique_match() {
        let dir = tempdir("edit");
        std::fs::write(dir.join("a.txt"), "x = 1\nx = 1\n").unwrap();
        let out = edit_file(
            &ctx(&dir),
            &json!({"path": "a.txt", "old_string": "x = 1", "new_string": "x = 2"}),
        );
        assert!(out.is_error);
        assert!(out.text.contains("2 times"));

        let out = edit_file(
            &ctx(&dir),
            &json!({"path": "a.txt", "old_string": "x = 1\nx = 1", "new_string": "x = 2"}),
        );
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "x = 2\n"
        );
    }

    #[test]
    fn grep_finds_literal_matches() {
        let dir = tempdir("grep");
        std::fs::write(dir.join("a.rs"), "fn main() {}\n// TODO: fix\n").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing\n").unwrap();
        let out = grep_files(&ctx(&dir), &json!({"pattern": "TODO", "include": "*.rs"}));
        assert!(!out.is_error);
        assert!(out.text.contains("a.rs:2:"));
        let count = grep_files(&ctx(&dir), &json!({"pattern": "TODO", "mode": "count"}));
        assert!(count.text.starts_with("1 matching lines"));
    }

    #[test]
    fn read_file_truncation_maps_the_remainder() {
        let dir = tempdir("readmap");
        let mut src = String::new();
        for i in 0..2050 {
            src.push_str(&format!("// filler line {i}\n"));
        }
        src.push_str("pub fn late_function(x: u32) -> u32 {\n    x\n}\n");
        std::fs::write(dir.join("big.rs"), &src).unwrap();
        let out = read_file(&ctx(&dir), &json!({"path": "big.rs"}));
        assert!(!out.is_error);
        assert!(
            out.text.contains("continue with start_line=2001"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("declarations in the unread part"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("pub fn late_function(x: u32) -> u32"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("2051"),
            "map carries the jump line: {}",
            out.text
        );
    }

    #[test]
    fn grep_annotates_enclosing_declaration() {
        let dir = tempdir("grepenc");
        std::fs::write(
            dir.join("a.rs"),
            "pub fn outer() {\n    let marker = 1;\n}\nconst MARKER_TOP: u32 = 2;\n",
        )
        .unwrap();
        let out = grep_files(&ctx(&dir), &json!({"pattern": "marker"}));
        assert!(!out.is_error);
        assert!(out.text.contains("a.rs:2:"), "{}", out.text);
        assert!(out.text.contains("[in pub fn outer]"), "{}", out.text);
    }

    #[test]
    fn glob_matches_bare_and_nested() {
        let dir = tempdir("glob");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("top.md"), "").unwrap();
        let out = glob_files(&ctx(&dir), &json!({"pattern": "*.rs"}));
        assert!(
            out.text.contains("src/lib.rs"),
            "bare pattern matches at depth: {}",
            out.text
        );
        let out = glob_files(&ctx(&dir), &json!({"pattern": "src/**/*.rs"}));
        assert!(out.text.contains("src/lib.rs"));
        let count = glob_files(&ctx(&dir), &json!({"pattern": "*.md", "mode": "count"}));
        assert_eq!(count.text, "1 matching paths");
    }
}
