//! Structural outlines without a parser dependency: a two-pass scanner that
//! strips comments and strings while tracking brace depth (indentation for
//! Python), then matches declaration heads to recover signatures, nesting,
//! and line spans. Powers the code_outline tool, the remainder map appended
//! to truncated read_file output, and the enclosing-declaration notes on
//! grep_files hits.

use super::{ToolContext, ToolOutcome};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;
use walkdir::WalkDir;

const SIG_CHAR_CAP: usize = 300;
const SIG_ROW_CAP: usize = 8;
const ROW_CHAR_CAP: usize = 500;
const STRING_KEEP: usize = 40;
const PARSE_ITEM_CAP: usize = 500;
const FILE_ITEM_CAP: usize = 300;
const FILE_BYTE_CAP: usize = 24 * 1024;
const DIR_FILE_CAP: usize = 80;
const DIR_ITEM_CAP: usize = 20;
const DIR_BYTE_CAP: usize = 32 * 1024;
const OUTLINE_SOURCE_CAP: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    JsTs,
    Python,
    Go,
    CFamily,
}

impl Lang {
    pub fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => Lang::JsTs,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "swift"
            | "cs" | "scala" => Lang::CFamily,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::JsTs => "JS/TS",
            Lang::Python => "Python",
            Lang::Go => "Go",
            Lang::CFamily => "C-family",
        }
    }
}

/// What a declaration can contain, which decides whether the lines nested
/// under it are scanned for further declarations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// No children reported (fn, const, type alias, ...).
    Leaf,
    /// Children matched by the ident-then-paren member heuristic too
    /// (class bodies, Go interfaces).
    Class,
    /// Children matched by keyword rules only (impl, mod, namespace, describe).
    Group,
}

pub struct Item {
    /// 1-based line the declaration starts on.
    pub line: usize,
    /// 1-based last line of the body (best effort; = line for bodyless items).
    pub end: usize,
    pub depth: usize,
    pub text: String,
}

/// One source line after comment/string stripping. `depth` is the brace
/// depth at the start of the line.
struct Row {
    depth: usize,
    text: String,
}

// ---------------------------------------------------------------- lexing

enum Mode {
    Code,
    /// Block comment; the count allows Rust's nesting.
    Block(u32),
    Str {
        quote: char,
        /// JS template literal: `${` opens an expression counted here.
        template: bool,
        expr: i32,
        /// Rust raw string: 1 + number of #s, 0 for a normal string.
        raw_hashes: u8,
        /// Content characters kept so far (capped at STRING_KEEP).
        emitted: usize,
    },
}

fn push_string_char(out: &mut String, c: char, emitted: &mut usize) {
    // Delimiters that would confuse downstream balance counting stay out.
    if matches!(c, '(' | ')' | '{' | '}' | ';') {
        return;
    }
    if *emitted < STRING_KEEP && out.len() < ROW_CHAR_CAP {
        out.push(c);
        *emitted += 1;
        if *emitted == STRING_KEEP {
            out.push('…');
        }
    }
}

/// Append a code character unless the row is already at its display cap; the
/// scan itself keeps going so brace depth stays correct on long lines.
fn push_code(out: &mut String, c: char) {
    if out.len() < ROW_CHAR_CAP {
        out.push(c);
    }
}

/// Lex a brace-language source into cleaned rows with brace depth.
fn scan_brace(lang: Lang, text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut depth: i32 = 0;
    let mut mode = Mode::Code;
    for line in text.lines() {
        let start_depth = depth.max(0) as usize;
        let mut out = String::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match &mut mode {
                Mode::Code => {
                    let c = chars[i];
                    let next = chars.get(i + 1).copied();
                    if c == '/' && next == Some('/') {
                        break;
                    }
                    if c == '/' && next == Some('*') {
                        mode = Mode::Block(1);
                        i += 2;
                        continue;
                    }
                    if c == '{' {
                        depth += 1;
                        push_code(&mut out, c);
                        i += 1;
                        continue;
                    }
                    if c == '}' {
                        depth -= 1;
                        push_code(&mut out, c);
                        i += 1;
                        continue;
                    }
                    if c == '\'' && lang == Lang::Rust {
                        // Char literal or lifetime: 'a' is a literal only if
                        // it closes within two chars; otherwise a lifetime.
                        if next == Some('\\') {
                            push_code(&mut out, '…');
                            i += 2;
                            while i < chars.len() && chars[i] != '\'' {
                                i += 1;
                            }
                            i += 1;
                        } else if chars.get(i + 2) == Some(&'\'') {
                            push_code(&mut out, '…');
                            i += 3;
                        } else {
                            push_code(&mut out, '\'');
                            i += 1;
                        }
                        continue;
                    }
                    if lang == Lang::Rust && c == 'r' && matches!(next, Some('"') | Some('#')) {
                        let mut hashes = 0u8;
                        let mut j = i + 1;
                        while chars.get(j) == Some(&'#') {
                            hashes = hashes.saturating_add(1);
                            j += 1;
                        }
                        if chars.get(j) == Some(&'"') {
                            mode = Mode::Str {
                                quote: '"',
                                template: false,
                                expr: 0,
                                raw_hashes: hashes.saturating_add(1),
                                emitted: 0,
                            };
                            push_code(&mut out, '"');
                            i = j + 1;
                            continue;
                        }
                    }
                    if c == '"' || c == '\'' || c == '`' {
                        mode = Mode::Str {
                            quote: c,
                            template: c == '`' && lang == Lang::JsTs,
                            expr: 0,
                            raw_hashes: 0,
                            emitted: 0,
                        };
                        push_code(&mut out, c);
                        i += 1;
                        continue;
                    }
                    push_code(&mut out, c);
                    i += 1;
                }
                Mode::Block(n) => {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        *n -= 1;
                        i += 2;
                        if *n == 0 {
                            mode = Mode::Code;
                        }
                    } else if lang == Lang::Rust
                        && chars[i] == '/'
                        && chars.get(i + 1) == Some(&'*')
                    {
                        *n += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::Str { quote, template, expr, raw_hashes, emitted } => {
                    let c = chars[i];
                    if *raw_hashes > 0 {
                        if c == '"' {
                            let need = (*raw_hashes - 1) as usize;
                            let closed = chars[i + 1..]
                                .iter()
                                .take(need)
                                .filter(|&&h| h == '#')
                                .count()
                                == need;
                            if closed {
                                push_code(&mut out, '"');
                                mode = Mode::Code;
                                i += 1 + need;
                                continue;
                            }
                        }
                        push_string_char(&mut out, c, emitted);
                        i += 1;
                        continue;
                    }
                    if *expr > 0 {
                        // Inside `${...}`: content dropped, braces balanced.
                        if c == '{' {
                            *expr += 1;
                        } else if c == '}' {
                            *expr -= 1;
                        }
                        i += 1;
                        continue;
                    }
                    if *template && c == '$' && chars.get(i + 1) == Some(&'{') {
                        *expr = 1;
                        i += 2;
                        continue;
                    }
                    if c == '\\' && !(*quote == '`' && lang == Lang::Go) {
                        i += 2;
                        continue;
                    }
                    if c == *quote {
                        push_code(&mut out, c);
                        mode = Mode::Code;
                        i += 1;
                        continue;
                    }
                    push_string_char(&mut out, c, emitted);
                    i += 1;
                }
            }
        }
        // Ordinary quotes don't survive the newline; an unbalanced one (a
        // regex, invalid code) must not poison the rest of the file. Rust raw
        // strings, Go raw strings and JS templates legitimately span lines.
        if let Mode::Str { quote, raw_hashes, .. } = mode {
            if quote != '`' && raw_hashes == 0 {
                mode = Mode::Code;
            }
        }
        rows.push(Row { depth: start_depth, text: out });
    }
    rows
}

// --------------------------------------------------------------- matching

/// `kw` at the head of `s` as a whole word: returns the rest, trimmed.
fn word<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if c.is_alphanumeric() || c == '_' => None,
        Some(_) => Some(rest.trim_start()),
    }
}

/// Strip leading modifiers (`pub(crate)`, `export`, `static`, ...) so the
/// declaration keyword is at the head. A paren group or a quoted ABI right
/// after a modifier is skipped with it.
fn strip_mods<'a>(mut s: &'a str, mods: &[&str]) -> &'a str {
    loop {
        let mut stripped = false;
        for m in mods {
            if let Some(rest) = word(s, m) {
                let rest = if let Some(inner) = rest.strip_prefix('(') {
                    inner.split_once(')').map(|x| x.1.trim_start()).unwrap_or(rest)
                } else if let Some(inner) = rest.strip_prefix('"') {
                    inner.split_once('"').map(|x| x.1.trim_start()).unwrap_or(rest)
                } else {
                    rest
                };
                s = rest;
                stripped = true;
                break;
            }
        }
        if !stripped {
            return s;
        }
    }
}

/// An identifier followed by `(` — the member heuristic used inside class
/// bodies and Go interfaces.
fn ident_then_paren(s: &str) -> bool {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || c == '_' || c == '$' || c == '~' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return false;
    }
    const RESERVED: [&str; 12] = [
        "if", "else", "for", "while", "switch", "catch", "return", "new", "typeof", "await",
        "super", "do",
    ];
    if RESERVED.contains(&&s[..end]) {
        return false;
    }
    s[end..].trim_start().starts_with('(')
}

fn starts_call(s: &str, name: &str) -> bool {
    if let Some(rest) = s.strip_prefix(name) {
        rest.starts_with('(')
            || rest.strip_prefix('.').is_some_and(|r| {
                r.starts_with("only") || r.starts_with("each") || r.starts_with("skip")
            })
    } else {
        false
    }
}

fn match_rust(s: &str) -> Option<Kind> {
    let s = strip_mods(s, &["pub", "unsafe", "async", "extern", "default", "const"]);
    for kw in ["trait", "impl", "mod"] {
        if word(s, kw).is_some() {
            return Some(Kind::Group);
        }
    }
    for kw in ["fn", "struct", "enum", "union", "type", "static"] {
        if word(s, kw).is_some() {
            return Some(Kind::Leaf);
        }
    }
    // `const` was consumed as a modifier above (for `const fn`); a plain
    // constant now looks like `NAME: Type = ...` — recover it from the raw
    // head instead.
    if s.starts_with("macro_rules!") {
        return Some(Kind::Leaf);
    }
    None
}

/// `const NAME: ...` — separated from match_rust because `const` doubles as
/// the `const fn` modifier there.
fn match_rust_const(s: &str) -> bool {
    let s = strip_mods(s, &["pub"]);
    word(s, "const")
        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_'))
}

fn match_jsts(s: &str, parent: Option<Kind>) -> Option<Kind> {
    for kw in ["describe", "context"] {
        if starts_call(s, kw) {
            return Some(Kind::Group);
        }
    }
    for kw in ["it", "test"] {
        if starts_call(s, kw) {
            return Some(Kind::Leaf);
        }
    }
    let t = strip_mods(s, &["export", "default", "declare", "abstract", "async"]);
    if word(t, "function").is_some() || t.starts_with("function*") {
        return Some(Kind::Leaf);
    }
    if word(t, "class").is_some() {
        return Some(Kind::Class);
    }
    if word(t, "namespace").is_some() || word(t, "module").is_some() {
        return Some(Kind::Group);
    }
    for kw in ["interface", "enum"] {
        if word(t, kw).is_some() {
            return Some(Kind::Leaf);
        }
    }
    if let Some(rest) = word(t, "type") {
        if rest.contains('=') {
            return Some(Kind::Leaf);
        }
    }
    for kw in ["const", "let", "var"] {
        if let Some(rest) = word(t, kw) {
            if let Some((_, after)) = rest.split_once('=') {
                let after = after.trim_start();
                if after.starts_with('(')
                    || after.starts_with("async")
                    || after.starts_with("function")
                    || rest.contains("=>")
                {
                    return Some(Kind::Leaf);
                }
            }
        }
    }
    if parent == Some(Kind::Class) {
        let m = strip_mods(
            s,
            &[
                "public", "private", "protected", "static", "readonly", "async", "get", "set",
                "override", "abstract", "accessor",
            ],
        )
        .trim_start_matches('*')
        .trim_start();
        if ident_then_paren(m) {
            return Some(Kind::Leaf);
        }
    }
    None
}

fn match_go(s: &str, parent: Option<Kind>) -> Option<Kind> {
    if word(s, "func").is_some() {
        return Some(Kind::Leaf);
    }
    if let Some(rest) = word(s, "type") {
        if rest.contains("interface") {
            return Some(Kind::Class);
        }
        return Some(Kind::Leaf);
    }
    if parent.is_none() && (word(s, "const").is_some() || word(s, "var").is_some()) {
        return Some(Kind::Leaf);
    }
    if parent == Some(Kind::Class) && ident_then_paren(s) {
        return Some(Kind::Leaf);
    }
    None
}

/// C-family declarations need a post-check: the ident-then-paren heuristic
/// also matches call statements, so the caller keeps a hit only when the
/// joined signature ends in `{` (a body) or reads like a prototype.
fn match_cfamily(s: &str) -> Option<(Kind, bool)> {
    if s.starts_with("#define") {
        return Some((Kind::Leaf, false));
    }
    if s.starts_with('#') || s.starts_with("template") {
        return None;
    }
    let t = strip_mods(
        s,
        &[
            "public", "private", "protected", "static", "final", "abstract", "virtual", "inline",
            "constexpr", "extern", "export", "friend", "internal", "sealed", "partial",
            "override", "open", "data",
        ],
    );
    if word(t, "namespace").is_some() {
        return Some((Kind::Group, false));
    }
    for kw in ["class", "struct", "interface"] {
        if word(t, kw).is_some() {
            return Some((Kind::Class, false));
        }
    }
    for kw in ["enum", "union", "typedef", "fun", "func"] {
        if word(t, kw).is_some() {
            return Some((Kind::Leaf, false));
        }
    }
    if let Some(rest) = word(t, "using") {
        if !rest.starts_with("namespace") {
            return Some((Kind::Leaf, false));
        }
        return None;
    }
    const CONTROL: [&str; 15] = [
        "if", "else", "for", "while", "switch", "do", "case", "return", "break", "continue",
        "goto", "throw", "try", "catch", "delete",
    ];
    for kw in CONTROL {
        if word(t, kw).is_some() {
            return None;
        }
    }
    let starts_ident = t.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '~');
    if starts_ident && t.contains('(') && !t.split('(').next().unwrap_or("").contains('=') {
        return Some((Kind::Leaf, true));
    }
    None
}

// ---------------------------------------------------------------- parsing

/// Join a declaration head that may span rows into one signature, cut at the
/// body `{` or the trailing `;`. Returns (signature, rows consumed,
/// terminator).
fn join_signature(rows: &[Row], start: usize) -> (String, usize, Option<char>) {
    let mut sig = String::new();
    let mut balance = 0i32;
    let mut consumed = 0;
    let mut terminator = None;
    'rows: for (k, row) in rows[start..].iter().take(SIG_ROW_CAP).enumerate() {
        let t = row.text.trim();
        if !sig.is_empty() {
            sig.push(' ');
        }
        consumed = k + 1;
        for c in t.chars() {
            match c {
                '(' | '[' => balance += 1,
                ')' | ']' => balance -= 1,
                '{' => {
                    terminator = Some('{');
                    break 'rows;
                }
                ';' if balance <= 0 => {
                    terminator = Some(';');
                    break 'rows;
                }
                _ => {}
            }
            sig.push(c);
            if sig.len() > SIG_CHAR_CAP {
                sig.push('…');
                break 'rows;
            }
        }
        let continues = t.ends_with([',', '(', '=', '+', '|', '&', '<'])
            || t.ends_with("->")
            || t.ends_with("=>")
            || t.ends_with("where");
        if balance <= 0 && !continues {
            break;
        }
    }
    let sig = sig.split_whitespace().collect::<Vec<_>>().join(" ");
    (sig, consumed.max(1), terminator)
}

fn parse_brace(lang: Lang, rows: &[Row]) -> Vec<Item> {
    let mut items: Vec<(Item, usize)> = Vec::new(); // (item, 0-based last sig row)
    let mut flags: [Option<Kind>; 2] = [None, None];
    let mut i = 0;
    while i < rows.len() && items.len() < PARSE_ITEM_CAP {
        let depth = rows[i].depth;
        let trimmed = rows[i].text.trim();
        let parent = match depth {
            0 => None,
            1 => flags[0].filter(|k| *k != Kind::Leaf),
            2 if flags[0].is_some_and(|k| k != Kind::Leaf) => {
                flags[1].filter(|k| *k != Kind::Leaf)
            }
            _ => None,
        };
        let allowed = depth == 0 || parent.is_some();
        if trimmed.is_empty()
            || !allowed
            || (lang == Lang::Rust && trimmed.starts_with("#["))
        {
            i += 1;
            continue;
        }
        let matched: Option<(Kind, bool)> = match lang {
            Lang::Rust => match_rust(trimmed)
                .or_else(|| match_rust_const(trimmed).then_some(Kind::Leaf))
                .map(|k| (k, false)),
            Lang::JsTs => match_jsts(trimmed, parent).map(|k| (k, false)),
            Lang::Go => match_go(trimmed, parent).map(|k| (k, false)),
            Lang::CFamily => match_cfamily(trimmed),
            Lang::Python => None,
        };
        let Some((kind, needs_body_check)) = matched else {
            i += 1;
            continue;
        };
        let (sig, consumed, terminator) = join_signature(rows, i);
        if needs_body_check {
            let proto = terminator == Some(';')
                && sig.split('(').next().unwrap_or("").split_whitespace().count() >= 2;
            if terminator != Some('{') && !proto {
                i += consumed;
                continue;
            }
        }
        if depth < 2 {
            flags[depth] = Some(kind);
            if depth == 0 {
                flags[1] = None;
            }
        }
        items.push((Item { line: i + 1, end: i + 1, depth, text: sig }, i + consumed - 1));
        i += consumed;
    }
    close_items(rows, items)
}

/// Best-effort end lines: an item's body runs until brace depth returns to
/// the item's own depth.
fn close_items(rows: &[Row], items: Vec<(Item, usize)>) -> Vec<Item> {
    items
        .into_iter()
        .map(|(mut item, sig_end)| {
            let mut started = false;
            item.end = sig_end + 1;
            for (k, row) in rows.iter().enumerate().skip(sig_end + 1) {
                if row.depth > item.depth {
                    started = true;
                } else if started {
                    item.end = k; // 1-based line of the closing-brace row
                    break;
                } else if row.text.trim_start().starts_with('{') {
                    started = true; // Allman-style brace on its own line
                } else if !row.text.trim().is_empty() {
                    break; // no body ever opened
                }
                if k == rows.len() - 1 && started {
                    item.end = rows.len();
                }
            }
            item
        })
        .collect()
}

fn parse_python(text: &str) -> Vec<Item> {
    // (indent, cleaned) per line; usize::MAX marks lines inside triple quotes.
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut triple: Option<char> = None;
    for line in text.lines() {
        if let Some(q) = triple {
            let close = q.to_string().repeat(3);
            if line.contains(&close) {
                triple = None;
            }
            lines.push((usize::MAX, String::new()));
            continue;
        }
        let mut indent = 0usize;
        for c in line.chars() {
            match c {
                ' ' => indent += 1,
                '\t' => indent += 4,
                _ => break,
            }
        }
        let mut out = String::new();
        let chars: Vec<char> = line.trim_start().chars().collect();
        let mut i = 0;
        let mut in_str: Option<char> = None;
        let mut emitted = 0usize;
        while i < chars.len() && out.len() < ROW_CHAR_CAP {
            let c = chars[i];
            if let Some(q) = in_str {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    out.push(c);
                    in_str = None;
                } else {
                    push_string_char(&mut out, c, &mut emitted);
                }
                i += 1;
                continue;
            }
            if c == '#' {
                break;
            }
            if c == '"' || c == '\'' {
                if chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c) {
                    // Triple quote: closed on this line, or spills over.
                    let close_at = (i + 3..chars.len().saturating_sub(2)).find(|&j| {
                        chars[j] == c && chars[j + 1] == c && chars[j + 2] == c
                    });
                    match close_at {
                        Some(j) => {
                            out.push('…');
                            i = j + 3;
                            continue;
                        }
                        None => {
                            triple = Some(c);
                            break;
                        }
                    }
                }
                out.push(c);
                in_str = Some(c);
                emitted = 0;
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
        lines.push((indent, out));
    }

    let mut items: Vec<Item> = Vec::new();
    // (indent, is_class, index into items or usize::MAX for unreported defs)
    let mut stack: Vec<(usize, bool, usize)> = Vec::new();
    let mut last_code_line = 0usize;
    let mut i = 0;
    while i < lines.len() && items.len() < PARSE_ITEM_CAP {
        let (indent, ref cleaned) = lines[i];
        let trimmed = cleaned.trim();
        if indent == usize::MAX || trimmed.is_empty() {
            i += 1;
            continue;
        }
        while stack.last().is_some_and(|top| top.0 >= indent) {
            let (_, _, idx) = stack.pop().unwrap();
            if idx != usize::MAX {
                items[idx].end = last_code_line;
            }
        }
        last_code_line = i + 1;
        let is_class = word(trimmed, "class").is_some();
        let is_def = word(trimmed, "def").is_some()
            || word(trimmed, "async").is_some_and(|r| word(r, "def").is_some());
        if is_class || is_def {
            let depth = stack.len();
            let all_class_parents = stack.iter().all(|s| s.1);
            let report = depth == 0 || (depth == 1 && all_class_parents);
            // Join until the `:` that ends the header, tracking (), [], {}.
            let mut sig = String::new();
            let mut balance = 0i32;
            let mut consumed = 1;
            'rows: for (k, (ind, row)) in lines[i..].iter().take(SIG_ROW_CAP).enumerate() {
                if *ind == usize::MAX {
                    break;
                }
                let t = row.trim();
                if !sig.is_empty() {
                    sig.push(' ');
                }
                consumed = k + 1;
                for c in t.chars() {
                    match c {
                        '(' | '[' | '{' => balance += 1,
                        ')' | ']' | '}' => balance -= 1,
                        ':' if balance <= 0 => break 'rows,
                        _ => {}
                    }
                    sig.push(c);
                    if sig.len() > SIG_CHAR_CAP {
                        sig.push('…');
                        break 'rows;
                    }
                }
                if balance <= 0 {
                    break;
                }
            }
            let sig = sig.split_whitespace().collect::<Vec<_>>().join(" ");
            let idx = if report {
                items.push(Item { line: i + 1, end: i + 1, depth, text: sig });
                items.len() - 1
            } else {
                usize::MAX
            };
            stack.push((indent, is_class, idx));
            i += consumed;
            continue;
        }
        // Module-level constant: SCREAMING_CASE assignment.
        if stack.is_empty() {
            if let Some((name, _)) = trimmed.split_once('=') {
                let name = name.trim().trim_end_matches(':').trim();
                let is_const = !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    && name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
                if is_const {
                    let mut text: String = trimmed.chars().take(60).collect();
                    if trimmed.chars().count() > 60 {
                        text.push('…');
                    }
                    items.push(Item { line: i + 1, end: i + 1, depth: 0, text });
                }
            }
        }
        i += 1;
    }
    let total = lines.len();
    for (_, _, idx) in stack {
        if idx != usize::MAX {
            items[idx].end = last_code_line.max(1).min(total);
        }
    }
    items
}

pub fn outline(lang: Lang, text: &str) -> Vec<Item> {
    match lang {
        Lang::Python => parse_python(text),
        _ => parse_brace(lang, &scan_brace(lang, text)),
    }
}

/// The innermost declaration whose span contains `line`, if any.
pub fn enclosing(items: &[Item], line: usize) -> Option<&Item> {
    items.iter().filter(|i| i.line <= line && line <= i.end).max_by_key(|i| i.line)
}

/// Compact name for annotations: the signature up to its parameter list.
pub fn short_name(text: &str) -> String {
    let head = text.split('(').next().unwrap_or(text).trim();
    let head = if head.is_empty() { text } else { head };
    let mut out: String = head.chars().take(48).collect();
    if head.chars().count() > 48 {
        out.push('…');
    }
    out
}

pub fn render(items: &[Item], max_items: usize, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    for item in items {
        if shown >= max_items || out.len() >= max_bytes {
            break;
        }
        let _ = writeln!(out, "{:>6}  {}{}", item.line, "  ".repeat(item.depth), item.text);
        shown += 1;
    }
    if shown < items.len() {
        let _ = write!(out, "[{shown} of {} declarations shown]", items.len());
    }
    out
}

// ------------------------------------------------------------------- tool

pub fn code_outline(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let path = input["path"].as_str().unwrap_or(".");
    let resolved = ctx.resolve(path);
    if resolved.is_dir() {
        return outline_dir(&resolved);
    }
    let Some(lang) = Lang::of(&resolved) else {
        let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("?");
        return ToolOutcome::err(format!(
            "no structural scanner for .{ext} files — use read_file instead"
        ));
    };
    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::err(format!("cannot read {path}: {e}")),
    };
    let items = outline(lang, &text);
    let rel = super::fs::display_rel(ctx, &resolved);
    if items.is_empty() {
        return ToolOutcome::ok(format!("{rel}: no declarations found"));
    }
    let mut out = format!(
        "{rel} — {}, {} lines, {} declarations\n",
        lang.name(),
        text.lines().count(),
        items.len()
    );
    out.push_str(&render(&items, FILE_ITEM_CAP, FILE_BYTE_CAP));
    ToolOutcome::ok(out)
}

fn outline_dir(root: &Path) -> ToolOutcome {
    let mut files: Vec<std::path::PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir() && super::fs::skip_dir(&e.file_name().to_string_lossy()))
        })
        .flatten()
        .filter(|e| {
            e.file_type().is_file()
                && Lang::of(e.path()).is_some()
                && e.metadata().map(|m| m.len() <= OUTLINE_SOURCE_CAP).unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();
    if files.is_empty() {
        return ToolOutcome::ok("no supported source files found".to_string());
    }
    let total = files.len();
    let mut out = String::new();
    let mut shown = 0usize;
    for file in &files {
        if shown >= DIR_FILE_CAP || out.len() >= DIR_BYTE_CAP {
            break;
        }
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        let Some(lang) = Lang::of(file) else { continue };
        let items = outline(lang, &text);
        let rel = file.strip_prefix(root).unwrap_or(file).display().to_string();
        shown += 1;
        if items.is_empty() {
            let _ = writeln!(out, "## {rel} ({} lines) — no declarations", text.lines().count());
            continue;
        }
        let _ = writeln!(out, "## {rel} ({} lines)", text.lines().count());
        out.push_str(&render(&items, DIR_ITEM_CAP, DIR_BYTE_CAP.saturating_sub(out.len())));
    }
    if shown < total {
        let _ = write!(out, "[{shown} of {total} source files shown; point me at a subdirectory]");
    }
    ToolOutcome::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[Item]) -> Vec<(usize, usize, &str)> {
        items.iter().map(|i| (i.line, i.depth, i.text.as_str())).collect()
    }

    #[test]
    fn rust_outline_declarations_and_nesting() {
        let src = r####"use std::fmt;

const CAP: usize = 10;

pub fn top(a: &str) -> String {
    let s = "fn fake() {"; // string and comment stay out
    s.to_string()
}

pub struct Thing {
    field: u32,
}

impl Thing {
    pub fn method(&self) -> u32 {
        self.field
    }
    fn helper<'a>(&'a self) -> &'a u32 {
        let raw = r#"brace { inside raw"#;
        &self.field
    }
}

mod tests {
    fn nested() {}
}
"####;
        let items = outline(Lang::Rust, src);
        let got = texts(&items);
        assert_eq!(
            got,
            vec![
                (3, 0, "const CAP: usize = 10"),
                (5, 0, "pub fn top(a: &str) -> String"),
                (10, 0, "pub struct Thing"),
                (14, 0, "impl Thing"),
                (15, 1, "pub fn method(&self) -> u32"),
                (18, 1, "fn helper<'a>(&'a self) -> &'a u32"),
                (24, 0, "mod tests"),
                (25, 1, "fn nested()"),
            ]
        );
        // spans: `top` ends at its closing brace, `impl Thing` spans its body
        let top = items.iter().find(|i| i.text.contains("fn top")).unwrap();
        assert_eq!((top.line, top.end), (5, 8));
        let imp = items.iter().find(|i| i.text.starts_with("impl")).unwrap();
        assert_eq!((imp.line, imp.end), (14, 22));
    }

    #[test]
    fn rust_multiline_signature_joins() {
        let src = "pub fn long(\n    a: usize,\n    b: usize,\n) -> usize {\n    a + b\n}\n";
        let items = outline(Lang::Rust, src);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "pub fn long( a: usize, b: usize, ) -> usize");
        assert_eq!(items[0].line, 1);
        assert_eq!(items[0].end, 6);
    }

    #[test]
    fn jsts_outline_classes_arrows_and_suites() {
        let src = "\
export const handler = async (req) => {
  const inner = () => {};
  return req;
};

export class Store {
  constructor(db) {
    this.db = db;
  }
  async load(id) {
    return this.db.get(`key ${id} {brace}`);
  }
}

interface Config {
  retries: number;
}

describe('store', () => {
  it('loads by id', () => {
    expect(load(1)).toBe(1);
  });
});
";
        let items = outline(Lang::JsTs, src);
        let got = texts(&items);
        assert_eq!(
            got,
            vec![
                (1, 0, "export const handler = async (req) =>"),
                (6, 0, "export class Store"),
                (7, 1, "constructor(db)"),
                (10, 1, "async load(id)"),
                (15, 0, "interface Config"),
                (19, 0, "describe('store', () =>"),
                (20, 1, "it('loads by id', () =>"),
            ]
        );
    }

    #[test]
    fn python_outline_indentation_and_strings() {
        let src = "\
LIMIT = 25

def helper(x):
    def closure(y):
        return y
    return closure

class Runner:
    '''docstring with
def fake(self):
    '''

    def run(self,
            retries=3):
        return retries

CODE = '''
def also_fake():
    pass
'''
";
        let items = outline(Lang::Python, src);
        let got = texts(&items);
        assert_eq!(
            got,
            vec![
                (1, 0, "LIMIT = 25"),
                (3, 0, "def helper(x)"),
                (8, 0, "class Runner"),
                (13, 1, "def run(self, retries=3)"),
                (17, 0, "CODE ="),
            ]
        );
        let runner = items.iter().find(|i| i.text.starts_with("class")).unwrap();
        assert!(runner.end >= 15, "class span reaches its last method, got {}", runner.end);
    }

    #[test]
    fn go_outline_funcs_types_and_interfaces() {
        let src = "\
package main

const (
    a = 1
    b = 2
)

type Store struct {
    db string
}

type Reader interface {
    Read(p []byte) (n int, err error)
}

func (s *Store) Load(id string) error {
    return nil
}

func main() {}
";
        let items = outline(Lang::Go, src);
        let got = texts(&items);
        assert_eq!(
            got,
            vec![
                (3, 0, "const ( a = 1 b = 2 )"),
                (8, 0, "type Store struct"),
                (12, 0, "type Reader interface"),
                (13, 1, "Read(p []byte) (n int, err error)"),
                (16, 0, "func (s *Store) Load(id string) error"),
                (20, 0, "func main()"),
            ]
        );
    }

    #[test]
    fn cfamily_outline_keeps_definitions_drops_calls() {
        let src = "\
#define MAX 10

static int add(int a, int b) {
    helper(a);
    return a + b;
}

void proto(int x);

class Point {
public:
    int norm() const;
};

setup(registry);
";
        let items = outline(Lang::CFamily, src);
        let got = texts(&items);
        assert_eq!(
            got,
            vec![
                (1, 0, "#define MAX 10"),
                (3, 0, "static int add(int a, int b)"),
                (8, 0, "void proto(int x)"),
                (10, 0, "class Point"),
                (12, 1, "int norm() const"),
            ]
        );
    }

    #[test]
    fn enclosing_returns_innermost() {
        let src = "impl Thing {\n    fn method(&self) {\n        body();\n    }\n}\n";
        let items = outline(Lang::Rust, src);
        let hit = enclosing(&items, 3).unwrap();
        assert_eq!(short_name(&hit.text), "fn method");
        assert!(enclosing(&items, 99).is_none());
    }

    #[test]
    fn code_outline_tool_file_dir_and_unsupported() {
        let dir = std::env::temp_dir().join(format!("odei-test-outline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("sub/b.py"), "def beta():\n    pass\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "just text\n").unwrap();
        let ctx = ToolContext::new(&dir);

        let file = code_outline(&ctx, &serde_json::json!({"path": "a.rs"}));
        assert!(!file.is_error);
        assert!(file.text.contains("pub fn alpha()"), "{}", file.text);

        let tree = code_outline(&ctx, &serde_json::json!({}));
        assert!(!tree.is_error);
        assert!(tree.text.contains("## a.rs"), "{}", tree.text);
        assert!(tree.text.contains("def beta()"), "{}", tree.text);
        assert!(!tree.text.contains("notes.txt"));

        let bad = code_outline(&ctx, &serde_json::json!({"path": "notes.txt"}));
        assert!(bad.is_error);
        assert!(bad.text.contains(".txt"));
    }
}
