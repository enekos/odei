use crate::tools::{fs, outline, ToolContext};
use regex::Regex;
use serde_json::json;
use std::sync::OnceLock;

const TOTAL_BYTE_CAP: usize = 64 * 1024;
const MAX_ATTACHMENTS: usize = 10;

pub struct Attachment {
    pub mention: String,
    pub kind: &'static str,
    pub summary: String,
    pub body: String,
}

fn pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("@\"([^\"\n]+)\"|@([^\\s\"]+)").expect("mention pattern"))
}

fn trim_trailing_punctuation(token: &str) -> &str {
    token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}'])
}

pub fn scan(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for capture in pattern().captures_iter(text) {
        let whole = capture.get(0).expect("whole match");
        let standalone = text[..whole.start()]
            .chars()
            .next_back()
            .map(char::is_whitespace)
            .unwrap_or(true);
        if !standalone {
            continue;
        }
        let token = match capture.get(1) {
            Some(quoted) => quoted.as_str().to_string(),
            None => {
                trim_trailing_punctuation(capture.get(2).expect("bare match").as_str()).to_string()
            }
        };
        if token.is_empty() || token == "." || found.contains(&token) {
            continue;
        }
        found.push(token);
    }
    found
}

fn char_boundary_at_or_before(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn expand(ctx: &ToolContext, text: &str) -> (String, Vec<Attachment>) {
    let mentions = scan(text);
    if mentions.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut budget = TOTAL_BYTE_CAP;
    for mention in mentions {
        if attachments.len() >= MAX_ATTACHMENTS || budget == 0 {
            break;
        }
        let resolved = ctx.resolve(&mention);
        let Ok(meta) = std::fs::metadata(&resolved) else {
            continue;
        };
        let (kind, outcome) = if meta.is_dir() {
            let mapped = outline::code_outline(ctx, &json!({ "path": mention }));
            if mapped.is_error || mapped.text.trim().is_empty() {
                ("listing", fs::list_files(ctx, &json!({ "path": mention })))
            } else {
                ("map", mapped)
            }
        } else {
            ("file", fs::read_file(ctx, &json!({ "path": mention })))
        };
        if outcome.is_error {
            continue;
        }
        let mut body = outcome.text;
        if body.len() > budget {
            body.truncate(char_boundary_at_or_before(&body, budget));
            body.push_str("\n[attachment cut to fit; read_file returns the rest]");
        }
        budget = budget.saturating_sub(body.len());
        let summary = format!("{} lines", body.lines().count());
        attachments.push(Attachment {
            mention,
            kind,
            summary,
            body,
        });
    }
    if attachments.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::from(text);
    out.push_str(
        "\n\n---\n\n# Attached by the user\n\nThe message above points at these paths with @. \
         Their contents as of this turn follow; you have already read them.\n",
    );
    for attachment in &attachments {
        out.push_str(&format!(
            "\n## @{} ({})\n\n{}\n",
            attachment.mention,
            attachment.kind,
            attachment.body.trim_end()
        ));
    }
    (out, attachments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_is_a_path_at_the_start_of_a_word() {
        assert_eq!(scan("look at @src/ui.rs please"), vec!["src/ui.rs"]);
        assert_eq!(scan("@Cargo.toml"), vec!["Cargo.toml"]);
        assert_eq!(scan("compare @a.rs and @b.rs"), vec!["a.rs", "b.rs"]);
        assert_eq!(scan("@src/ui.rs, then @src/ui.rs again"), vec!["src/ui.rs"]);
        assert_eq!(scan("mail me at eneko@example.com"), Vec::<String>::new());
        assert_eq!(scan("read @\"my notes.md\" now"), vec!["my notes.md"]);
        assert_eq!(scan("nothing here"), Vec::<String>::new());
    }

    #[test]
    fn a_file_comes_in_whole_and_a_directory_comes_in_as_a_map() {
        let dir = std::env::temp_dir().join(format!("odei-test-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "pub fn hello() {}\n").unwrap();
        let ctx = ToolContext::new(&dir);

        let (prompt, attached) = expand(&ctx, "explain @src/lib.rs");
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].kind, "file");
        assert!(prompt.starts_with("explain @src/lib.rs"), "{prompt}");
        assert!(prompt.contains("## @src/lib.rs (file)"), "{prompt}");
        assert!(prompt.contains("pub fn hello"), "{prompt}");

        let (_, mapped) = expand(&ctx, "what is in @src");
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].kind, "map");
        assert!(mapped[0].body.contains("hello"), "{}", mapped[0].body);

        let (untouched, none) = expand(&ctx, "look at @does/not/exist");
        assert_eq!(untouched, "look at @does/not/exist");
        assert!(none.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
