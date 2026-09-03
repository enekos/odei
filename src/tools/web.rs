//! Web tools. web_fetch is a direct bounded HTTP GET with tag stripping;
//! web_search scrapes the DuckDuckGo HTML endpoint (no API key needed).

use super::{ToolContext, ToolOutcome};
use serde_json::Value;
use std::fmt::Write as _;
use std::io::Read as _;
use std::time::Duration;

const FETCH_BYTE_CAP: usize = 200 * 1024;
const FETCH_TEXT_CAP: usize = 60 * 1024;

fn get(url: &str) -> Result<(String, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .redirects(4)
        .build();
    let response = agent
        .get(url)
        .set("user-agent", "odei/0.1")
        .set(
            "accept",
            "text/html,application/xhtml+xml,text/plain,application/json,*/*",
        )
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            ureq::Error::Transport(t) => format!("network failure: {t}"),
        })?;
    let content_type = response.content_type().to_string();
    let mut reader = response.into_reader().take(FETCH_BYTE_CAP as u64 + 1);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|e| format!("read failed: {e}"))?;
    let truncated = bytes.len() > FETCH_BYTE_CAP;
    bytes.truncate(FETCH_BYTE_CAP);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n[response truncated]");
    }
    Ok((content_type, text))
}

/// Minimal HTML → text: drop script/style bodies, strip tags, decode the
/// common entities, and collapse blank runs.
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);
    // Remove script/style/noscript/svg blocks (the regex crate has no
    // backreferences, so spell the alternatives out).
    let block_re = regex::Regex::new(
        r"(?is)<script\b.*?</script>|<style\b.*?</style>|<noscript\b.*?</noscript>|<svg\b.*?</svg>",
    )
    .unwrap();
    let cleaned = block_re.replace_all(html, " ");
    let rest: &str = &cleaned;

    let mut in_tag = false;
    let mut last_was_newline = true;
    for c in rest.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                if in_tag {
                    in_tag = false;
                    if !last_was_newline {
                        text.push(' ');
                    }
                }
            }
            _ if in_tag => {}
            '\n' | '\r' => {
                if !last_was_newline {
                    text.push('\n');
                    last_was_newline = true;
                }
            }
            _ => {
                text.push(c);
                last_was_newline = false;
            }
        }
    }
    let entities = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&hellip;", "…"),
        ("&rsquo;", "'"),
        ("&lsquo;", "'"),
        ("&ldquo;", "\u{201C}"),
        ("&rdquo;", "\u{201D}"),
    ];
    let mut decoded = text;
    for (from, to) in entities {
        decoded = decoded.replace(from, to);
    }
    // Collapse runs of spaces and blank lines.
    let space_re = regex::Regex::new(r"[ \t]{2,}").unwrap();
    let decoded = space_re.replace_all(&decoded, " ");
    let blank_re = regex::Regex::new(r"\n{3,}").unwrap();
    blank_re.replace_all(&decoded, "\n\n").trim().to_string()
}

pub fn web_fetch(_ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(url) = input["url"].as_str() else {
        return ToolOutcome::err("missing required field: url");
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return ToolOutcome::err("url must be http(s)");
    }
    let cap = input["max_bytes"]
        .as_u64()
        .map(|v| (v as usize).min(FETCH_TEXT_CAP))
        .unwrap_or(FETCH_TEXT_CAP);
    match get(url) {
        Ok((content_type, body)) => {
            let mut text = if content_type.contains("html") {
                html_to_text(&body)
            } else {
                body
            };
            if text.len() > cap {
                text.truncate(cap);
                text.push_str("\n[truncated]");
            }
            ToolOutcome::ok(format!(
                "Untrusted content from {url} ({content_type}):\n\n{text}"
            ))
        }
        Err(e) => ToolOutcome::err(format!("cannot fetch {url}: {e}")),
    }
}

pub fn web_search(_ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let Some(query) = input["query"].as_str() else {
        return ToolOutcome::err("missing required field: query");
    };
    let allowed: Vec<String> = input["allowed_domains"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let blocked: Vec<String> = input["blocked_domains"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let encoded: String = query
        .bytes()
        .flat_map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                vec![b as char]
            } else if b == b' ' {
                vec!['+']
            } else {
                format!("%{b:02X}").chars().collect()
            }
        })
        .collect();
    let url = format!("https://html.duckduckgo.com/html/?q={encoded}");
    let body = match get(&url) {
        Ok((_, body)) => body,
        Err(e) => return ToolOutcome::err(format!("search failed: {e}")),
    };

    // Result anchors look like: <a class="result__a" href="...">Title</a>
    let link_re = regex::Regex::new(
        r#"(?is)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let mut out = String::new();
    let mut count = 0usize;
    for capture in link_re.captures_iter(&body) {
        let mut href = capture[1].to_string();
        // DDG wraps results as //duckduckgo.com/l/?uddg=<encoded>&rut=...
        if let Some(pos) = href.find("uddg=") {
            let tail = &href[pos + 5..];
            let end = tail.find('&').unwrap_or(tail.len());
            href = percent_decode(&tail[..end]);
        }
        if !href.starts_with("http") {
            continue;
        }
        let domain = href.split('/').nth(2).unwrap_or("");
        if !allowed.is_empty() && !allowed.iter().any(|d| domain.ends_with(d.as_str())) {
            continue;
        }
        if blocked.iter().any(|d| domain.ends_with(d.as_str())) {
            continue;
        }
        let title = html_to_text(&capture[2]);
        let _ = writeln!(out, "- {title}\n  {href}");
        count += 1;
        if count >= 8 {
            break;
        }
    }
    if count == 0 {
        ToolOutcome::ok(format!("no results for {query:?}"))
    } else {
        ToolOutcome::ok(format!(
            "Untrusted web search results for {query:?}:\n\n{out}"
        ))
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
