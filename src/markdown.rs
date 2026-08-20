//! Markdown for a terminal, rendered while it streams.
//!
//! The model writes markdown, so the shell reads markdown: headings, lists,
//! quotes, rules, fenced code, tables, and the inline span set (`code`,
//! **strong**, *emphasis*, ~~strike~~, links). Two constraints shape the
//! design:
//!
//! * **It renders as it arrives.** Text is not held until the end of the
//!   turn — a line is classified as soon as its shape is certain and then
//!   streamed through a wrapping [`Flow`], so a paragraph appears word by
//!   word the way raw text used to. Only three things are ever buffered: the
//!   first ~24 characters of a line (a pipe there might make it a table row),
//!   a table, whose column widths cannot be known until the last row is in,
//!   and the newline at the end of a paragraph line, which is only a break if
//!   the next line does not continue it.
//! * **Unfinished markup is not guessed at.** When a span opens and its
//!   closer has not arrived, output pauses at the marker rather than
//!   optimistically styling the rest of the line; if the line ends unclosed,
//!   the marker is printed literally. So `*.rs` and `2 * 3` survive, and
//!   `**bold**` is never left bolding the remainder of a paragraph. A span
//!   that ends exactly where the buffer does waits one more character, since
//!   `[a b]` becomes a link the moment a `(` arrives.
//!
//! Models hard-wrap their prose at a measure of their own, so a source
//! newline inside a paragraph is treated as the space it stands for and the
//! paragraph is re-wrapped to the terminal — otherwise every answer would
//! break twice, once at their width and once at ours.
//!
//! With `NO_COLOR`, a redirected stdout, or `ODEI_MARKDOWN=off`, the whole
//! module degrades to a passthrough: piped output stays byte-for-byte the
//! text the model produced.

use crate::theme::Theme;

/// Widest prose measure, however wide the terminal is. Long lines are harder
/// to read than narrow ones, and an answer is prose, not a table.
const MEASURE: usize = 96;
/// Tables may use more of the screen than prose, up to this.
const TABLE_MAX: usize = 120;
/// Characters buffered before a line is assumed to be a paragraph. A table
/// row without a leading pipe (`Model | Notes`) declares itself well inside
/// this, and 24 characters is a token or two of latency.
const SNIFF: usize = 24;
/// Left margin for code and table bodies.
const PAD: &str = "  ";
/// Narrowest a table column is squeezed to before the table is allowed to
/// overflow instead.
const MIN_COL: usize = 6;
const BULLETS: [&str; 4] = ["•", "◦", "▪", "·"];

// ------------------------------------------------------------------ widths

/// Terminal columns a string occupies: wide (East Asian, emoji) characters
/// count two, combining marks none.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if is_combining(c) {
                0
            } else if is_wide(c) {
                2
            } else if (c as u32) < 0x20 {
                0
            } else {
                1
            }
        })
        .sum()
}

fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x20D0..=0x20FF | 0xFE00..=0xFE0F)
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD)
}

/// Drop escape sequences, leaving what the eye sees.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: runs until a byte in @–~.
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs until BEL or ESC \.
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Visible width of an already-styled string.
pub fn visible_width(s: &str) -> usize {
    display_width(&strip_ansi(s))
}

// -------------------------------------------------------------------- flow

/// A line being written: where the cursor is, what styles are open, and what
/// to print again after a wrap. Words accumulate in `chunk` until whitespace
/// arrives, because a word's width is only known once it is whole.
struct Flow<'t> {
    theme: &'t Theme,
    width: usize,
    /// Printed after every wrap — the bullet's hanging indent, the quote bar.
    lead: String,
    lead_width: usize,
    col: usize,
    styles: Vec<&'static str>,
    chunk: String,
    chunk_width: usize,
    space: bool,
}

impl<'t> Flow<'t> {
    fn new(theme: &'t Theme, width: usize) -> Flow<'t> {
        Flow {
            theme,
            width,
            lead: String::new(),
            lead_width: 0,
            col: 0,
            styles: Vec::new(),
            chunk: String::new(),
            chunk_width: 0,
            space: false,
        }
    }

    fn with_lead(theme: &'t Theme, width: usize, lead: String, lead_width: usize) -> Flow<'t> {
        let mut flow = Flow::new(theme, width);
        flow.lead = lead;
        flow.lead_width = lead_width;
        flow.col = lead_width;
        flow
    }

    fn styles_str(&self) -> String {
        self.styles.concat()
    }

    /// A style that stays open for the whole line (a heading, a quote).
    fn base(&mut self, style: &'static str) {
        self.open(style);
    }

    fn open(&mut self, style: &'static str) {
        self.chunk.push_str(style);
        self.styles.push(style);
    }

    fn close(&mut self) {
        self.styles.pop();
        self.chunk.push_str(self.theme.reset());
        let styles = self.styles_str();
        self.chunk.push_str(&styles);
    }

    fn put(&mut self, text: &str, width: usize) {
        self.chunk.push_str(text);
        self.chunk_width += width;
    }

    fn put_char(&mut self, c: char) {
        self.chunk.push(c);
        self.chunk_width += display_width(&c.to_string());
    }

    /// Whitespace: the word so far can be placed, and the next one may go on
    /// the following line.
    fn brk(&mut self, out: &mut String) {
        self.flush(out);
        self.space = true;
    }

    fn flush(&mut self, out: &mut String) {
        if self.chunk.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.chunk);
        let width = std::mem::take(&mut self.chunk_width);
        self.place(&text, width, out);
    }

    fn place(&mut self, text: &str, width: usize, out: &mut String) {
        if self.space {
            if self.col > self.lead_width && self.col + 1 + width > self.width {
                self.wrap(out);
            } else {
                out.push(' ');
                self.col += 1;
                self.space = false;
            }
        }
        if self.col > self.lead_width && self.col + width > self.width {
            self.wrap(out);
        }
        out.push_str(text);
        self.col += width;
    }

    fn wrap(&mut self, out: &mut String) {
        out.push_str(self.theme.reset());
        out.push('\n');
        out.push_str(&self.lead);
        let styles = self.styles_str();
        out.push_str(&styles);
        self.col = self.lead_width;
        self.space = false;
    }
}

// ------------------------------------------------------------------ inline

/// What starts at a given position: a complete span, a construct still
/// waiting for its closer, or nothing special.
enum Span {
    /// The character is literal.
    None,
    /// A construct opened here and has not closed yet.
    Hold,
    Escape {
        ch: char,
        next: usize,
    },
    Code {
        content: (usize, usize),
        next: usize,
    },
    Emph {
        content: (usize, usize),
        next: usize,
        strong: bool,
        em: bool,
        strike: bool,
    },
    Link {
        text: (usize, usize),
        dest: (usize, usize),
        next: usize,
        image: bool,
    },
    Auto {
        dest: (usize, usize),
        next: usize,
    },
}

type Chars = [(usize, char)];

fn run(cs: &Chars, i: usize, ch: char) -> usize {
    cs[i..].iter().take_while(|(_, c)| *c == ch).count()
}

fn slice<'a>(src: &'a str, cs: &Chars, range: (usize, usize)) -> &'a str {
    let start = cs.get(range.0).map(|(b, _)| *b).unwrap_or(src.len());
    let end = cs.get(range.1).map(|(b, _)| *b).unwrap_or(src.len());
    if start <= end {
        &src[start..end]
    } else {
        ""
    }
}

fn is_md_punct(c: char) -> bool {
    "\\`*_~[]()#+-.!|<>{}\"'".contains(c)
}

/// End of the code span opening at `i` (index just past its closing run).
fn code_end(cs: &Chars, i: usize) -> Option<usize> {
    let k = run(cs, i, '`');
    let mut j = i + k;
    while j < cs.len() {
        if cs[j].1 == '`' {
            let m = run(cs, j, '`');
            if m == k {
                return Some(j);
            }
            j += m;
        } else {
            j += 1;
        }
    }
    None
}

/// Index of the closing run for an emphasis opener of `k` `ch`s.
fn find_closer(cs: &Chars, from: usize, ch: char, k: usize) -> Option<usize> {
    let mut j = from;
    while j < cs.len() {
        let c = cs[j].1;
        if c == '\\' {
            j += 2;
            continue;
        }
        // A marker inside a code span closes nothing.
        if c == '`' {
            match code_end(cs, j) {
                Some(end) => {
                    j = end + run(cs, end, '`');
                    continue;
                }
                None => return None,
            }
        }
        if c == ch {
            let m = run(cs, j, ch);
            let tight = j > from && !cs[j - 1].1.is_whitespace();
            let intraword =
                ch == '_' && cs.get(j + m).is_some_and(|(_, n)| n.is_alphanumeric() || *n == '_');
            if m >= k && tight && !intraword {
                return Some(j);
            }
            j += m;
            continue;
        }
        j += 1;
    }
    None
}

/// Matching bracket for the one at `open`, skipping code spans and escapes.
fn bracket_end(cs: &Chars, open: usize, close_ch: char) -> Option<usize> {
    let open_ch = cs[open].1;
    let mut depth = 0usize;
    let mut j = open;
    while j < cs.len() {
        let c = cs[j].1;
        if c == '\\' {
            j += 2;
            continue;
        }
        if c == '`' {
            match code_end(cs, j) {
                Some(end) => {
                    j = end + run(cs, end, '`');
                    continue;
                }
                None => return None,
            }
        }
        if c == open_ch {
            depth += 1;
        } else if c == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

fn link_at(cs: &Chars, bracket: usize, image: bool) -> Span {
    let Some(close) = bracket_end(cs, bracket, ']') else { return Span::Hold };
    match cs.get(close + 1).map(|(_, c)| *c) {
        Some('(') => {}
        // Nothing after `]` yet: a `(` may be the next character to arrive.
        // A partial line holds; a finished one has its answer, and it is no.
        None => return Span::Hold,
        Some(_) => return Span::None,
    }
    let Some(paren) = bracket_end(cs, close + 1, ')') else { return Span::Hold };
    Span::Link {
        text: (bracket + 1, close),
        dest: (close + 2, paren),
        next: paren + 1,
        image,
    }
}

fn auto_at(cs: &Chars, i: usize) -> Span {
    let mut j = i + 1;
    let mut scheme = false;
    while j < cs.len() {
        let c = cs[j].1;
        if c == '>' {
            return if scheme && j > i + 1 {
                Span::Auto { dest: (i + 1, j), next: j + 1 }
            } else {
                Span::None
            };
        }
        if c == ':' {
            scheme = j > i + 1;
            j += 1;
            continue;
        }
        let ok = if scheme {
            !c.is_whitespace()
        } else {
            c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')
        };
        if !ok {
            return Span::None;
        }
        j += 1;
    }
    // Still plausible: a scheme, or the letters of one, and no closer yet.
    if scheme || j > i + 1 {
        Span::Hold
    } else {
        Span::None
    }
}

fn span_at(cs: &Chars, i: usize) -> Span {
    let n = cs.len();
    let c = cs[i].1;
    match c {
        '\\' => match cs.get(i + 1) {
            None => Span::Hold,
            Some((_, d)) if is_md_punct(*d) => Span::Escape { ch: *d, next: i + 2 },
            Some(_) => Span::None,
        },
        '`' => match code_end(cs, i) {
            Some(end) => {
                let k = run(cs, i, '`');
                Span::Code { content: (i + k, end), next: end + k }
            }
            None => Span::Hold,
        },
        '*' | '_' | '~' => {
            let mut k = run(cs, i, c);
            if c == '~' {
                if k < 2 {
                    return Span::None;
                }
                k = 2;
            } else {
                k = k.min(3);
            }
            match cs.get(i + k) {
                None => return Span::Hold,
                Some((_, a)) if a.is_whitespace() => return Span::None,
                Some(_) => {}
            }
            // `snake_case` is a word, not emphasis.
            if c == '_' && i > 0 && (cs[i - 1].1.is_alphanumeric() || cs[i - 1].1 == '_') {
                return Span::None;
            }
            match find_closer(cs, i + k, c, k) {
                Some(j) => {
                    let (strong, em, strike) = match (c, k) {
                        ('~', _) => (false, false, true),
                        (_, 1) => (false, true, false),
                        (_, 2) => (true, false, false),
                        _ => (true, true, false),
                    };
                    Span::Emph { content: (i + k, j), next: j + k, strong, em, strike }
                }
                None => Span::Hold,
            }
        }
        '!' if i + 1 < n && cs[i + 1].1 == '[' => link_at(cs, i + 1, true),
        '[' => link_at(cs, i, false),
        '<' => auto_at(cs, i),
        _ => Span::None,
    }
}

/// Render inline markup into `flow`. Unclosed constructs are printed as the
/// literal characters they are.
fn inline(src: &str, flow: &mut Flow, out: &mut String) {
    let cs: Vec<(usize, char)> = src.char_indices().collect();
    let theme = flow.theme;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i].1;
        match span_at(&cs, i) {
            Span::Escape { ch, next } => {
                flow.put_char(ch);
                i = next;
            }
            Span::Code { content, next } => {
                let text = slice(src, &cs, content);
                flow.open(theme.code);
                flow.put(text, display_width(text));
                flow.close();
                i = next;
            }
            Span::Emph { content, next, strong, em, strike } => {
                let mut opened = 0;
                for (on, style) in
                    [(strong, theme.strong), (em, theme.emphasis), (strike, theme.strike)]
                {
                    if on {
                        flow.open(style);
                        opened += 1;
                    }
                }
                inline(slice(src, &cs, content), flow, out);
                for _ in 0..opened {
                    flow.close();
                }
                i = next;
            }
            Span::Link { text, dest, next, image } => {
                let label = slice(src, &cs, text);
                let url = slice(src, &cs, dest).split_whitespace().next().unwrap_or("");
                let url = url.trim_start_matches('<').trim_end_matches('>');
                flow.open(if image { theme.dim } else { theme.link });
                if label.is_empty() {
                    flow.put(url, display_width(url));
                } else {
                    inline(label, flow, out);
                }
                flow.close();
                // The destination is kept — a terminal link you cannot copy
                // is worse than one you can see. Anchors and self-links add
                // nothing, so they stay out.
                let worth_showing = !url.is_empty()
                    && !url.starts_with('#')
                    && url != label
                    && !label.is_empty();
                if worth_showing {
                    flow.brk(out);
                    flow.open(theme.dim);
                    flow.put_char('(');
                    flow.put(url, display_width(url));
                    flow.put_char(')');
                    flow.close();
                }
                i = next;
            }
            Span::Auto { dest, next } => {
                let url = slice(src, &cs, dest);
                flow.open(theme.link);
                flow.put(url, display_width(url));
                flow.close();
                i = next;
            }
            Span::None | Span::Hold => {
                if c.is_whitespace() {
                    flow.brk(out);
                } else {
                    flow.put_char(c);
                }
                i += 1;
            }
        }
    }
}

/// Longest prefix of a partial line whose rendering will not change when more
/// text arrives: everything up to an unfinished construct.
fn safe_split(src: &str) -> usize {
    let cs: Vec<(usize, char)> = src.char_indices().collect();
    let at = |i: usize| cs.get(i).map(|(b, _)| *b).unwrap_or(src.len());
    let mut i = 0;
    while i < cs.len() {
        // A marker at the very end may still be the first character of a
        // longer one — `~` of `~~`, `!` of `![`. Hold rather than commit it
        // to being literal.
        if i + 1 == cs.len() && "\\`*_~[!<".contains(cs[i].1) {
            return at(i);
        }
        let next = match span_at(&cs, i) {
            Span::Hold => return at(i),
            // An escape is two characters and cannot be reinterpreted.
            Span::Escape { next, .. } => {
                i = next;
                continue;
            }
            Span::Code { next, .. }
            | Span::Emph { next, .. }
            | Span::Link { next, .. }
            | Span::Auto { next, .. } => next,
            Span::None => {
                i += 1;
                continue;
            }
        };
        // A span that ends exactly at the tail is not settled: `[a b]`
        // becomes a link if a `(` arrives, `_x_` stops being emphasis if a
        // letter does. One more character decides it.
        if next >= cs.len() {
            return at(i);
        }
        i = next;
    }
    at(i)
}

/// Inline markup with every style dropped — what the text is worth in columns.
fn source_width(src: &str) -> usize {
    let mut flow = Flow::new(crate::theme::plain(), usize::MAX);
    let mut out = String::new();
    inline(src, &mut flow, &mut out);
    flow.flush(&mut out);
    display_width(&out)
}

// ------------------------------------------------------------------- lines

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// The shape of one source line.
enum Kind {
    Blank,
    Fence { ch: char, len: usize, lang: String },
    Rule,
    Heading(usize),
    Quote(usize),
    Bullet { indent: usize, task: Option<bool> },
    Ordered { indent: usize, marker: String },
    /// Possibly a table row — settled by whether the next line is a
    /// delimiter.
    Row,
    Para { indent: usize },
}

/// Classify a line. With `partial` set the line is still arriving, and `None`
/// means "cannot tell yet" — the caller keeps buffering. `rows` is cleared
/// for a line that has already been offered as a table row and turned down,
/// so it cannot be offered again.
fn classify(line: &str, partial: bool, rows: bool) -> Option<(Kind, usize)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if trimmed.is_empty() {
        return (!partial).then_some((Kind::Blank, 0));
    }
    // A pipe anywhere may make this a table row, and that is decided on the
    // whole line.
    if partial && trimmed.contains('|') {
        return None;
    }
    let first = trimmed.chars().next().unwrap_or(' ');

    if first == '`' || first == '~' {
        // A fence carries its language to the end of the line.
        if partial {
            return None;
        }
        let len = trimmed.chars().take_while(|c| *c == first).count();
        if len >= 3 {
            let lang = trimmed[len..].trim().to_string();
            return Some((Kind::Fence { ch: first, len, lang }, 0));
        }
    }

    if matches!(first, '-' | '*' | '_') {
        let bare = trimmed.chars().all(|c| c == first || c == ' ');
        if bare {
            // `---` is a rule, `- ` an empty bullet: only the whole line says.
            if partial {
                return None;
            }
            if trimmed.chars().filter(|c| *c == first).count() >= 3 {
                return Some((Kind::Rule, 0));
            }
        }
    }

    if first == '#' {
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        let rest = &trimmed[hashes..];
        if partial && rest.is_empty() {
            return None;
        }
        if hashes <= 6 && rest.starts_with(' ') {
            let text = rest.trim_start_matches(' ');
            return Some((Kind::Heading(hashes), line.len() - text.len()));
        }
    }

    if first == '>' {
        let mut rest = trimmed;
        let mut depth = 0;
        while let Some(next) = rest.strip_prefix('>') {
            depth += 1;
            rest = next.strip_prefix(' ').unwrap_or(next);
        }
        if rest.is_empty() {
            return (!partial).then_some((Kind::Quote(depth), line.len()));
        }
        return Some((Kind::Quote(depth), line.len() - rest.len()));
    }

    if matches!(first, '-' | '*' | '+') && trimmed[1..].starts_with(' ') {
        let rest = trimmed[1..].trim_start_matches(' ');
        // `- [x] ` is a checkbox; four more characters settle it.
        let task = if rest.starts_with('[') {
            let box_chars: Vec<char> = rest.chars().take(4).collect();
            if box_chars.len() < 4 {
                if partial {
                    return None;
                }
                None
            } else if box_chars[2] == ']' && box_chars[3] == ' ' {
                Some(box_chars[1] != ' ')
            } else {
                None
            }
        } else {
            None
        };
        let mut offset = line.len() - rest.len();
        if task.is_some() {
            // Past the box, counted in bytes — what is inside it need not be
            // one byte wide.
            offset += rest.char_indices().nth(4).map_or(rest.len(), |(at, _)| at);
        }
        return Some((Kind::Bullet { indent, task }, offset));
    }

    if first.is_ascii_digit() {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        let rest = &trimmed[digits..];
        let ordered = digits <= 9
            && matches!(rest.chars().next(), Some('.') | Some(')'))
            && rest[1..].starts_with(' ');
        if ordered {
            let marker = trimmed[..digits + 1].to_string();
            let content = rest[1..].trim_start_matches(' ');
            return Some((Kind::Ordered { indent, marker }, line.len() - content.len()));
        }
        if partial && rest.is_empty() {
            return None;
        }
    }

    if rows && !partial && is_row(trimmed) {
        return Some((Kind::Row, 0));
    }

    if partial && line.chars().count() < SNIFF {
        return None;
    }
    Some((Kind::Para { indent }, indent))
}

// ------------------------------------------------------------------ tables

/// Cells of a row, with the edge pipes and any escaped or code-span pipes
/// left alone.
fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let cs: Vec<char> = trimmed.chars().collect();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut fence = 0usize;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && i + 1 < cs.len() {
            cell.push(cs[i + 1]);
            i += 2;
            continue;
        }
        if c == '`' {
            let k = cs[i..].iter().take_while(|c| **c == '`').count();
            if fence == 0 {
                fence = k;
            } else if fence == k {
                fence = 0;
            }
            for _ in 0..k {
                cell.push('`');
            }
            i += k;
            continue;
        }
        if c == '|' && fence == 0 {
            cells.push(cell.trim().to_string());
            cell.clear();
            i += 1;
            continue;
        }
        cell.push(c);
        i += 1;
    }
    cells.push(cell.trim().to_string());
    if trimmed.starts_with('|') && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if trimmed.ends_with('|') && cells.len() > 1 && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

fn is_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.starts_with('|') || split_row(trimmed).len() >= 2
}

/// `| --- | :-: |` — the line that turns the row above it into a table, and
/// says how each column aligns.
fn delimiter(line: &str) -> Option<Vec<Align>> {
    let trimmed = line.trim();
    if !trimmed.contains('-') || !trimmed.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ')) {
        return None;
    }
    let cells = split_row(trimmed);
    if cells.is_empty() {
        return None;
    }
    let mut align = Vec::new();
    for cell in &cells {
        let core = cell.trim();
        let left = core.starts_with(':');
        let right = core.ends_with(':');
        let dashes = core.trim_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
            return None;
        }
        align.push(match (left, right) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(align)
}

/// Squeeze the widest columns until the table fits, but never below
/// [`MIN_COL`] — past that, overflow is more legible than a column of
/// one-letter fragments.
fn shrink(widths: &mut [usize], budget: usize) {
    let mut total: usize = widths.iter().sum();
    while total > budget {
        let Some(i) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_COL)
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
        else {
            break;
        };
        widths[i] -= 1;
        total -= 1;
    }
}

/// One cell as the visual lines it needs, each with its visible width.
fn cell_lines(src: &str, width: usize, theme: &Theme, header: bool) -> Vec<(String, usize)> {
    let mut flow = Flow::new(theme, width.max(1));
    let mut out = String::new();
    if header {
        flow.base(theme.table_header);
    }
    inline(src, &mut flow, &mut out);
    flow.flush(&mut out);
    out.split('\n').map(|line| (line.to_string(), visible_width(line))).collect()
}

// ---------------------------------------------------------------- renderer

enum Block {
    Text,
    Fence { ch: char, len: usize },
    Table { align: Vec<Align>, rows: Vec<Vec<String>> },
}

enum Open<'t> {
    None,
    Prose(Flow<'t>),
    Code,
}

/// Where the current line stands, so a decision can be made without holding
/// a borrow on the renderer.
enum Phase {
    Text,
    Fence(char),
    Table,
}

/// Markdown in, styled terminal output out — fed however the text arrives.
pub struct Renderer<'t> {
    theme: &'t Theme,
    measure: usize,
    table: usize,
    passthrough: bool,
    buf: String,
    block: Block,
    open: Open<'t>,
    /// Whether the open line is one a following plain line would continue.
    open_continues: bool,
    /// A paragraph line ended and the next line has not said yet whether it
    /// carries on — models hard-wrap their prose, so a source newline is
    /// usually a space, not a break.
    soft: bool,
    /// Where in `buf` the text after the soft break starts.
    soft_at: usize,
    /// A row waiting to learn whether the next line is a delimiter.
    header: Option<String>,
    /// A blank line owed to whatever prints next; collapses runs of them and
    /// keeps them off the end of the output.
    blank: bool,
    started: bool,
    tail_newline: bool,
}

impl<'t> Renderer<'t> {
    pub fn new(theme: &'t Theme) -> Renderer<'t> {
        let mut renderer = Renderer::sized(theme, term_width());
        if disabled_by_env() {
            renderer.passthrough = true;
        }
        renderer
    }

    fn sized(theme: &'t Theme, term: usize) -> Renderer<'t> {
        Renderer {
            theme,
            measure: term.saturating_sub(1).clamp(20, MEASURE),
            table: term.saturating_sub(1).clamp(20, TABLE_MAX),
            passthrough: !theme.enabled,
            buf: String::new(),
            block: Block::Text,
            open: Open::None,
            open_continues: false,
            soft: false,
            soft_at: 0,
            header: None,
            blank: false,
            started: false,
            tail_newline: true,
        }
    }

    /// True when nothing is half-rendered — no open line, no buffered block.
    pub fn idle(&self) -> bool {
        self.buf.is_empty()
            && matches!(self.open, Open::None)
            && self.header.is_none()
            && matches!(self.block, Block::Text)
            && !self.blank
    }

    /// Feed a streamed chunk; returns whatever is ready to print.
    pub fn push(&mut self, text: &str) -> String {
        if self.passthrough {
            self.started |= !text.is_empty();
            if let Some(last) = text.chars().last() {
                self.tail_newline = last == '\n';
            }
            return text.to_string();
        }
        let mut out = String::new();
        for ch in text.chars() {
            self.feed(ch, &mut out);
        }
        out
    }

    /// End of the model's text: close the open line, flush a buffered table,
    /// and forget everything so the next turn starts clean.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.passthrough {
            if self.started && !self.tail_newline {
                out.push('\n');
            }
        } else {
            // Nothing more is coming, so no line is left waiting on the next
            // one.
            self.close_out(&mut out);
            if let Some(header) = self.header.take() {
                // A row that never got its delimiter is just a line.
                self.whole_line(&header, false, &mut out);
                self.close_out(&mut out);
            }
            if matches!(self.block, Block::Table { .. }) {
                self.flush_table(&mut out);
            }
        }
        *self = Renderer {
            theme: self.theme,
            measure: self.measure,
            table: self.table,
            passthrough: self.passthrough,
            buf: String::new(),
            block: Block::Text,
            open: Open::None,
            open_continues: false,
            soft: false,
            soft_at: 0,
            header: None,
            blank: false,
            started: false,
            tail_newline: true,
        };
        out
    }

    /// Close whatever line is open, without waiting for a next one.
    fn close_out(&mut self, out: &mut String) {
        self.open_continues = false;
        if self.soft || !matches!(self.open, Open::None) || !self.buf.is_empty() {
            self.line_end(out);
        }
    }

    fn feed(&mut self, ch: char, out: &mut String) {
        match ch {
            '\r' => {}
            '\n' => self.line_end(out),
            // Tabs are a width the terminal and we would disagree about.
            '\t' => {
                self.buf.push_str("    ");
                self.advance(out);
            }
            // Nothing else may move the cursor but us.
            c if c.is_control() => {}
            c => {
                self.buf.push(c);
                self.advance(out);
            }
        }
    }

    fn phase(&self) -> Phase {
        match &self.block {
            Block::Text => Phase::Text,
            Block::Fence { ch, .. } => Phase::Fence(*ch),
            Block::Table { .. } => Phase::Table,
        }
    }

    /// Print as much of the partial line as is already certain.
    fn advance(&mut self, out: &mut String) {
        if self.soft {
            // Either the paragraph carries on — in which case the text is
            // already flowing again — or a new block has started and the
            // paragraph has just been closed.
            if !self.resolve_soft(true, out) || !matches!(self.open, Open::None) {
                return;
            }
        }
        if matches!(self.open, Open::None) {
            match self.phase() {
                // Rows are held: column widths need the whole table.
                Phase::Table => return,
                Phase::Fence(ch) => {
                    let trimmed = self.buf.trim_start();
                    // Could still turn out to be the closing fence.
                    if trimmed.is_empty() || trimmed.chars().all(|c| c == ch) {
                        return;
                    }
                    self.lead_blank(out);
                    self.started = true;
                    out.push_str(PAD);
                    out.push_str(self.theme.code_block);
                    self.open = Open::Code;
                }
                Phase::Text => {
                    if self.header.is_some() {
                        return;
                    }
                    let Some((kind, offset)) = classify(&self.buf, true, true) else { return };
                    self.begin(&kind, out);
                    self.buf.drain(..offset);
                }
            }
        }
        self.stream(out);
    }

    /// With a soft break pending, decide what the text after it is. Returns
    /// false while it is still too early to say — a plain line continues the
    /// paragraph, anything else ends it.
    fn resolve_soft(&mut self, partial: bool, out: &mut String) -> bool {
        let tail = self.buf[self.soft_at..].to_string();
        let Some((kind, _)) = classify(&tail, partial, true) else { return false };
        self.soft = false;
        if matches!(kind, Kind::Para { .. }) {
            self.stream(out);
        } else {
            // A heading, a list, a table, a blank: the paragraph is over.
            // Whatever was held at the break prints as the text it is.
            let held = self.buf[..self.soft_at].to_string();
            self.buf = tail;
            self.close_line(held.trim_end(), out);
        }
        true
    }

    fn stream(&mut self, out: &mut String) {
        match &mut self.open {
            Open::None => {}
            Open::Code => {
                out.push_str(&self.buf);
                self.buf.clear();
            }
            Open::Prose(flow) => {
                let cut = safe_split(&self.buf);
                if cut > 0 {
                    let text: String = self.buf.drain(..cut).collect();
                    inline(&text, flow, out);
                }
            }
        }
    }

    /// Print the line prefix (bullet, quote bar, heading style) and open a
    /// flow for its content.
    fn begin(&mut self, kind: &Kind, out: &mut String) {
        let theme = self.theme;
        if matches!(kind, Kind::Heading(_)) && self.started {
            self.blank = true;
        }
        self.lead_blank(out);
        self.started = true;
        // A heading is one line by definition; everything else takes a lazy
        // continuation.
        self.open_continues = !matches!(kind, Kind::Heading(_));
        self.open = match kind {
            Kind::Heading(level) => {
                let mut flow = Flow::new(theme, self.measure);
                flow.base(if *level <= 2 { theme.heading } else { theme.strong });
                Open::Prose(flow)
            }
            Kind::Quote(depth) => {
                let bar = "▌".repeat(*depth);
                let lead = format!("{}{bar}{} ", theme.quote_bar, theme.reset());
                out.push_str(&lead);
                let mut flow = Flow::with_lead(theme, self.measure, lead, depth + 1);
                flow.base(theme.quote);
                Open::Prose(flow)
            }
            Kind::Bullet { indent, task } => {
                // A checkbox is the marker; a bullet in front of it would say
                // nothing the box does not.
                let glyph = match task {
                    Some(true) => "☑",
                    Some(false) => "☐",
                    None => BULLETS[(indent / 2).min(BULLETS.len() - 1)],
                };
                let pad = " ".repeat(*indent);
                out.push_str(&pad);
                out.push_str(theme.bullet);
                out.push_str(glyph);
                out.push_str(theme.reset());
                out.push(' ');
                let width = indent + display_width(glyph) + 1;
                let lead = " ".repeat(width);
                Open::Prose(Flow::with_lead(theme, self.measure, lead, width))
            }
            Kind::Ordered { indent, marker } => {
                let pad = " ".repeat(*indent);
                out.push_str(&pad);
                out.push_str(theme.bullet);
                out.push_str(marker);
                out.push_str(theme.reset());
                out.push(' ');
                let width = indent + display_width(marker) + 1;
                Open::Prose(Flow::with_lead(theme, self.measure, " ".repeat(width), width))
            }
            Kind::Para { indent } => {
                let pad = " ".repeat(*indent);
                out.push_str(&pad);
                Open::Prose(Flow::with_lead(theme, self.measure, pad, *indent))
            }
            // Block kinds never open a line.
            _ => Open::None,
        };
        if matches!(self.open, Open::None) {
            self.open_continues = false;
        }
    }

    fn close_line(&mut self, rest: &str, out: &mut String) {
        match &mut self.open {
            Open::Prose(flow) => {
                inline(rest, flow, out);
                flow.flush(out);
            }
            Open::Code => out.push_str(rest),
            Open::None => {}
        }
        out.push_str(self.theme.reset());
        out.push('\n');
        self.open = Open::None;
        self.open_continues = false;
    }

    /// The open line has reached the end of a source line: hold it in case
    /// the next line is more of the same paragraph, or close it.
    fn end_open_line(&mut self, out: &mut String) {
        if self.open_continues {
            self.stream(out);
            if self.buf.is_empty() {
                if let Open::Prose(flow) = &mut self.open {
                    // Place the word in hand, then let the newline stand in
                    // for the space before the next one.
                    flow.brk(out);
                }
            } else {
                // Something is held mid-construct — a link whose target is on
                // the next line. Keep it, with the newline as the space.
                self.buf.push(' ');
            }
            self.soft = true;
            self.soft_at = self.buf.len();
            return;
        }
        let rest = std::mem::take(&mut self.buf);
        self.close_line(&rest, out);
    }

    fn line_end(&mut self, out: &mut String) {
        if self.soft {
            self.resolve_soft(false, out);
        }
        if !matches!(self.open, Open::None) {
            self.end_open_line(out);
            return;
        }
        let line = std::mem::take(&mut self.buf);
        match self.phase() {
            Phase::Fence(ch) => {
                let trimmed = line.trim();
                let len = match &self.block {
                    Block::Fence { len, .. } => *len,
                    _ => 3,
                };
                let closing = !trimmed.is_empty()
                    && trimmed.chars().all(|c| c == ch)
                    && trimmed.chars().count() >= len;
                if closing {
                    self.block = Block::Text;
                    self.blank = true;
                } else {
                    self.code_line(&line, out);
                }
            }
            Phase::Table => {
                if is_row(&line) {
                    if delimiter(&line).is_none() {
                        if let Block::Table { rows, .. } = &mut self.block {
                            rows.push(split_row(&line));
                        }
                    }
                } else {
                    self.flush_table(out);
                    self.text_line(&line, out);
                }
            }
            Phase::Text => self.text_line(&line, out),
        }
    }

    fn text_line(&mut self, line: &str, out: &mut String) {
        if let Some(header) = self.header.take() {
            if let Some(align) = delimiter(line) {
                self.block = Block::Table { align, rows: vec![split_row(&header)] };
                return;
            }
            // Just a line with a pipe in it after all. It is an ordinary
            // line, and this one follows it like any other — including
            // continuing it, if that is what it does.
            self.whole_line(&header, false, out);
            self.buf.push_str(line);
            self.line_end(out);
            return;
        }
        self.whole(line, out);
    }

    /// Render one complete line.
    fn whole(&mut self, line: &str, out: &mut String) {
        self.whole_line(line, true, out);
    }

    fn whole_line(&mut self, line: &str, rows: bool, out: &mut String) {
        let Some((kind, offset)) = classify(line, false, rows) else { return };
        match kind {
            Kind::Blank => self.blank = true,
            Kind::Fence { ch, len, lang } => {
                if self.started {
                    self.blank = true;
                }
                if !lang.is_empty() {
                    self.lead_blank(out);
                    self.started = true;
                    out.push_str(PAD);
                    out.push_str(self.theme.dim);
                    out.push_str(&lang);
                    out.push_str(self.theme.reset());
                    out.push('\n');
                }
                self.block = Block::Fence { ch, len };
            }
            Kind::Rule => {
                if self.started {
                    self.blank = true;
                }
                self.lead_blank(out);
                self.started = true;
                out.push_str(self.theme.divider);
                out.push_str(&"─".repeat(self.measure));
                out.push_str(self.theme.reset());
                out.push('\n');
                self.blank = true;
            }
            Kind::Row => self.header = Some(line.to_string()),
            other => {
                self.begin(&other, out);
                // The line arrived whole, but it may still be continued by
                // the next one, so it goes through the same ending as a
                // streamed line.
                self.buf = line[offset..].to_string();
                self.end_open_line(out);
            }
        }
    }

    fn code_line(&mut self, line: &str, out: &mut String) {
        self.lead_blank(out);
        self.started = true;
        if line.trim().is_empty() {
            out.push('\n');
            return;
        }
        out.push_str(PAD);
        out.push_str(self.theme.code_block);
        out.push_str(line);
        out.push_str(self.theme.reset());
        out.push('\n');
    }

    fn lead_blank(&mut self, out: &mut String) {
        if self.blank {
            self.blank = false;
            if self.started {
                out.push('\n');
            }
        }
    }

    fn flush_table(&mut self, out: &mut String) {
        let Block::Table { align, rows } =
            std::mem::replace(&mut self.block, Block::Text)
        else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let mut widths = vec![1usize; columns];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(source_width(cell));
            }
        }
        let separators = 3 * (columns - 1);
        let budget = self.table.saturating_sub(display_width(PAD) + separators);
        shrink(&mut widths, budget);

        if self.started {
            self.blank = true;
        }
        self.lead_blank(out);
        self.started = true;
        self.row(&rows[0], &widths, &align, true, out);
        out.push_str(PAD);
        out.push_str(self.theme.divider);
        let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
        out.push_str(&rule.join("─┼─"));
        out.push_str(self.theme.reset());
        out.push('\n');
        for row in &rows[1..] {
            self.row(row, &widths, &align, false, out);
        }
        self.blank = true;
    }

    fn row(
        &self,
        cells: &[String],
        widths: &[usize],
        align: &[Align],
        header: bool,
        out: &mut String,
    ) {
        let theme = self.theme;
        let rendered: Vec<Vec<(String, usize)>> = widths
            .iter()
            .enumerate()
            .map(|(i, width)| {
                let src = cells.get(i).map(String::as_str).unwrap_or("");
                cell_lines(src, *width, theme, header)
            })
            .collect();
        let height = rendered.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for line in 0..height {
            out.push_str(PAD);
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                    out.push_str(theme.divider);
                    out.push('│');
                    out.push_str(theme.reset());
                    out.push(' ');
                }
                let empty = (String::new(), 0);
                let (text, visible) = rendered[i].get(line).unwrap_or(&empty);
                let slack = width.saturating_sub(*visible);
                let last = i + 1 == widths.len();
                match align.get(i).copied().unwrap_or(Align::Left) {
                    Align::Left => {
                        out.push_str(text);
                        if !last {
                            out.push_str(&" ".repeat(slack));
                        }
                    }
                    Align::Right => {
                        out.push_str(&" ".repeat(slack));
                        out.push_str(text);
                    }
                    Align::Center => {
                        let left = slack / 2;
                        out.push_str(&" ".repeat(left));
                        out.push_str(text);
                        if !last {
                            out.push_str(&" ".repeat(slack - left));
                        }
                    }
                }
            }
            out.push_str(theme.reset());
            out.push('\n');
        }
    }
}

fn term_width() -> usize {
    crossterm::terminal::size().ok().map(|(w, _)| w as usize).filter(|w| *w >= 20).unwrap_or(80)
}

fn disabled_by_env() -> bool {
    matches!(
        std::env::var("ODEI_MARKDOWN").as_deref(),
        Ok("off") | Ok("0") | Ok("no") | Ok("none") | Ok("plain") | Ok("raw")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> &'static Theme {
        crate::theme::dark()
    }

    /// Render in one shot, as the eye would see it.
    fn seen(src: &str) -> String {
        let mut renderer = Renderer::sized(theme(), 60);
        let mut out = renderer.push(src);
        out.push_str(&renderer.finish());
        strip_ansi(&out)
    }

    /// Render arriving one character at a time.
    fn streamed(src: &str) -> String {
        let mut renderer = Renderer::sized(theme(), 60);
        let mut out = String::new();
        for ch in src.chars() {
            out.push_str(&renderer.push(&ch.to_string()));
        }
        out.push_str(&renderer.finish());
        out
    }

    /// Visible columns the column separators of a table line sit at. Every
    /// line of a table must agree on these.
    fn seps(line: &str) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut column = 0;
        for c in strip_ansi(line).chars() {
            if c == '│' || c == '┼' {
                columns.push(column);
            }
            column += display_width(&c.to_string());
        }
        columns
    }

    fn styled(src: &str) -> String {
        let mut renderer = Renderer::sized(theme(), 60);
        let mut out = renderer.push(src);
        out.push_str(&renderer.finish());
        out
    }

    #[test]
    fn inline_markup_becomes_styling_and_leaves_the_words() {
        let out = seen("A **bold** and *thin* and `code` and ~~gone~~ word.\n");
        assert_eq!(out, "A bold and thin and code and gone word.\n");
        // The escapes are really there.
        let raw = styled("A **bold** word\n");
        assert!(raw.contains("\x1b[1m"), "{raw:?}");
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(seen("use *.rs to match\n"), "use *.rs to match\n");
        assert_eq!(seen("2 * 3 * 4\n"), "2 * 3 * 4\n");
        assert_eq!(seen("snake_case_name here\n"), "snake_case_name here\n");
        assert_eq!(seen("a ** b\n"), "a ** b\n");
        // An opener with no closer prints as typed.
        assert_eq!(seen("**never closed\n"), "**never closed\n");
    }

    #[test]
    fn links_keep_their_destination() {
        assert_eq!(seen("see [the docs](https://kimi.com) now\n"), "see the docs (https://kimi.com) now\n");
        // A bare autolink is not doubled up.
        assert_eq!(seen("<https://kimi.com>\n"), "https://kimi.com\n");
        assert_eq!(seen("[https://a.io](https://a.io)\n"), "https://a.io\n");
    }

    #[test]
    fn headings_lists_quotes_and_rules_get_their_shape() {
        let out = seen("# Title\n\n- one\n- two\n  - deep\n1. first\n\n> quoted\n\n---\n");
        assert!(out.contains("Title\n"), "{out:?}");
        assert!(out.contains("• one\n"), "{out:?}");
        assert!(out.contains("  ◦ deep\n"), "{out:?}");
        assert!(out.contains("1. first\n"), "{out:?}");
        assert!(out.contains("▌ quoted\n"), "{out:?}");
        assert!(out.contains(&"─".repeat(59)), "{out:?}");
        // The `#` and `-` markers themselves are gone.
        assert!(!out.contains('#'), "{out:?}");
    }

    #[test]
    fn task_lists_become_boxes() {
        // The box is the marker; a bullet in front of it would add nothing.
        let out = seen("- [x] done\n- [ ] todo\n");
        assert_eq!(out, "☑ done\n☐ todo\n");
    }

    #[test]
    fn fenced_code_is_verbatim_and_indented() {
        let out = seen("text\n\n```rust\nlet a = *b; // **not bold**\n```\nafter\n");
        assert!(out.contains("  rust\n"), "{out:?}");
        assert!(out.contains("  let a = *b; // **not bold**\n"), "{out:?}");
        assert!(out.contains("after"), "{out:?}");
    }

    #[test]
    fn long_lines_wrap_under_their_own_indent() {
        let out = seen("- alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\n");
        let lines: Vec<&str> = out.trim_end().split('\n').collect();
        assert!(lines.len() > 1, "{out:?}");
        assert!(lines[0].starts_with("• alpha"), "{out:?}");
        // Continuations hang under the text, not the bullet.
        assert!(lines[1].starts_with("  "), "{out:?}");
        assert!(!lines[1].trim_start().is_empty());
        for line in &lines {
            assert!(display_width(line) <= 59, "{line:?} in {out:?}");
        }
    }

    #[test]
    fn tables_are_aligned_and_measured() {
        let src = "\
| Model | Notes |
| --- | ---: |
| `k3` | 1M context |
| kimi-for-coding | default |
";
        let out = seen(src);
        let lines: Vec<&str> = out.trim_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 4, "{out:?}");
        // Every line of the table puts its separators in the same columns.
        assert!(lines.iter().all(|l| seps(l) == seps(lines[0])), "{out:?}");
        assert!(lines[0].contains("Model"), "{out:?}");
        assert!(lines[1].contains('┼'), "{out:?}");
        // Right alignment pushes the cell against the separator.
        assert!(lines[2].ends_with("1M context"), "{out:?}");
        assert!(lines[3].contains("kimi-for-coding"), "{out:?}");
    }

    #[test]
    fn a_table_without_edge_pipes_is_still_a_table() {
        let out = seen("Model | Notes\n----- | -----\nk3 | fast\n");
        assert!(out.contains('│'), "{out:?}");
        assert!(out.contains("k3"), "{out:?}");
    }

    #[test]
    fn a_pipe_in_prose_is_not_a_table() {
        // No delimiter row follows, so the pipe is just a pipe — and the two
        // lines are one paragraph.
        let out = seen("run a | b to pipe it\nand then stop\n");
        assert_eq!(out, "run a | b to pipe it and then stop\n");
        // A blank line after it leaves the paragraph alone.
        let out = seen("run a | b to pipe it\n\n- next\n");
        assert_eq!(out, "run a | b to pipe it\n\n• next\n");
    }

    #[test]
    fn wide_tables_shrink_and_wrap_instead_of_overflowing() {
        let src = "\
| Column one | Column two |
| --- | --- |
| a phrase long enough to need folding across lines | another phrase that also needs folding here |
";
        let out = seen(src);
        let lines: Vec<&str> = out.trim_matches('\n').split('\n').collect();
        for line in &lines {
            assert!(display_width(line) <= 59, "{line:?} ({}) in {out:?}", display_width(line));
        }
        assert!(lines.iter().all(|l| seps(l) == seps(lines[0])), "{out:?}");
        // Nothing was dropped in the folding.
        assert!(out.contains("folding"), "{out:?}");
    }

    #[test]
    fn a_span_renders_wherever_it_falls_in_the_line() {
        // Past the first two dozen characters the line is already committed
        // to being a paragraph, so every span after that is settled one
        // arriving character at a time — the case that is easy to get wrong.
        for (markup, text) in [
            ("[a b](https://a.io)", "a b (https://a.io)"),
            ("**two words**", "two words"),
            ("`some code`", "some code"),
            ("*thin*", "thin"),
            ("~~gone~~", "gone"),
            ("<https://a.io>", "https://a.io"),
            ("_under_", "under"),
        ] {
            let src = format!("This line is long enough to have been committed {markup} tail\n");
            let mut renderer = Renderer::sized(theme(), 200);
            let mut out = renderer.push(&src);
            out.push_str(&renderer.finish());
            assert_eq!(
                strip_ansi(&out),
                format!("This line is long enough to have been committed {text} tail\n"),
                "{markup} did not survive being streamed"
            );
        }
    }

    #[test]
    fn hard_wrapped_prose_reflows_into_one_paragraph() {
        let out = seen("A sentence the model wrapped\nat its own measure, not ours.\n\nNext.\n");
        assert_eq!(
            out,
            "A sentence the model wrapped at its own measure, not ours.\n\nNext.\n"
        );
        // A list item takes its lazy continuation, and the next marker ends it.
        let out = seen("- one that carries\n  on below\n- two\n");
        assert_eq!(out, "• one that carries on below\n• two\n");
        // A link split across the break is still a link.
        let out = seen("see [the\ndocs](https://a.io) now\n");
        assert_eq!(out, "see the docs (https://a.io) now\n");
    }

    #[test]
    fn streaming_in_chunks_matches_a_single_push() {
        for src in [
            "# Title\n\nSome **bold** text with `code` and a [link](https://a.io).\n\n- one\n- two\n",
            "Model | Notes\n--- | ---\nk3 | fast\nkimi | default\n\ndone\n",
            "```sh\ncargo test --lib\n```\n\n> a quote that goes on for a while and wraps around\n",
            "A paragraph long enough that it has to wrap somewhere around here, plus *emphasis*.\n",
            "1. first item\n2. second item\n\n| a |\n| - |\n| b |\n",
        ] {
            assert_eq!(streamed(src), styled(src), "streaming differs for {src:?}");
        }
    }

    #[test]
    fn every_line_ends_reset_and_blank_runs_collapse() {
        let styled = styled("# H\n\n\n\ntext **b**\n\n\n");
        for line in styled.split('\n').filter(|l| !l.is_empty()) {
            assert!(line.ends_with("\x1b[0m"), "unterminated styling: {line:?}");
        }
        let seen = strip_ansi(&styled);
        assert!(!seen.contains("\n\n\n"), "{seen:?}");
        // No blank line is owed at the end.
        assert!(seen.ends_with("text b\n"), "{seen:?}");
    }

    #[test]
    fn plain_output_is_the_model_text_untouched() {
        let mut renderer = Renderer::sized(crate::theme::plain(), 60);
        let src = "# Title\n\n| a | b |\n| - | - |\n\n- **x**";
        let mut out = renderer.push(src);
        out.push_str(&renderer.finish());
        assert_eq!(out, format!("{src}\n"));
    }

    #[test]
    fn control_characters_from_the_model_cannot_move_the_cursor() {
        let out = styled("a\x1b[31mred\x07\n");
        assert!(!out.contains("\x1b[31m"), "{out:?}");
        assert_eq!(strip_ansi(&out), "a[31mred\n");
    }

    #[test]
    fn a_stream_that_stops_mid_line_still_closes_it() {
        let mut renderer = Renderer::sized(theme(), 60);
        let mut out = renderer.push("an unfinished thought with **bold");
        out.push_str(&renderer.finish());
        assert_eq!(strip_ansi(&out), "an unfinished thought with **bold\n");
        assert!(renderer.idle());
    }

    #[test]
    fn a_row_that_never_became_a_table_still_prints() {
        // The turn ends on what looked like a header. It is just a line.
        let mut renderer = Renderer::sized(theme(), 60);
        let mut out = renderer.push("a | b");
        out.push_str(&renderer.finish());
        assert_eq!(strip_ansi(&out), "a | b\n");
        assert!(out.ends_with('\n'), "{out:?}");
        assert!(renderer.idle());
    }

    #[test]
    fn a_table_at_the_very_end_is_still_drawn() {
        let out = seen("| a | b |\n| - | - |\n| 1 | 2 |");
        assert!(out.contains('┼'), "{out:?}");
        assert!(out.contains('1'), "{out:?}");
    }

    #[test]
    fn cells_can_carry_markup() {
        let out = seen("| a | b |\n| - | - |\n| `x` | **y** |\n");
        assert!(out.contains('x'), "{out:?}");
        assert!(!out.contains('`'), "{out:?}");
        assert!(!out.contains('*'), "{out:?}");
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        // A box with something other than x in it, wide characters in a
        // table, and an accent next to markup: all byte-index hazards.
        assert_eq!(seen("- [✓] done\n"), "☑ done\n");
        assert_eq!(seen("- [x] café\n"), "☑ café\n");
        // A column of double-width characters still lines up.
        let out = seen("| 日本語 | b |\n| - | - |\n| ✓ | 語 |\n");
        let lines: Vec<&str> = out.trim_matches('\n').split('\n').collect();
        assert!(lines.iter().all(|l| seps(l) == seps(lines[0])), "{out:?}");
        assert_eq!(seen("**é** and `ü`\n"), "é and ü\n");
    }

    #[test]
    fn widths_account_for_wide_characters() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(visible_width("\x1b[1mabc\x1b[0m"), 3);
    }
}
