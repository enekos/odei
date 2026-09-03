//! Context compaction.
//!
//! When a session outgrows the model's window, the older turns are replaced by
//! a single dense brief produced by the model itself. The cut is only ever
//! taken at a genuine user turn, so a tool_use block is never separated from
//! the tool_result that answers it — the protocol rejects that, and it is the
//! easy way to corrupt a transcript.

use crate::config::Config;
use crate::provider::{self, ContentBlock, Message};
use std::sync::atomic::AtomicBool;

/// User turns kept verbatim at the tail.
const KEEP_TURNS: usize = 2;
/// Per-result budget when rendering history for the summarizer.
const RESULT_EXCERPT: usize = 400;

const SUMMARY_SYSTEM: &str = "\
You are compacting the earlier part of a coding session so work can continue in less context.

Produce a dense brief, not prose, and keep it under 400 words. Preserve, in this order:

- what the user is trying to accomplish, and any constraint or preference they stated
- decisions that were made and the reason behind them
- every file path that was created, modified, or identified as relevant
- commands that were run and whether they passed or failed
- what is actually verified versus what is still assumed
- open blockers, and the next concrete step

Drop pleasantries, narration, superseded plans, and any file content that can simply be read
again. Keep identifiers, paths, error text, and stored-result handles (tr-...) exact — they are
referenced later. Write it for the assistant that picks the work up, not for a human reader.";

/// Is this a real user turn rather than a batch of tool results?
fn is_user_turn(message: &Message) -> bool {
    message.role == "user"
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Index to cut at, or None when there is not enough history to bother.
pub fn plan_cut(messages: &[Message]) -> Option<usize> {
    let boundaries: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_user_turn(m))
        .map(|(i, _)| i)
        .collect();
    if boundaries.len() <= KEEP_TURNS {
        return None;
    }
    let cut = boundaries[boundaries.len() - KEEP_TURNS];
    (cut > 0).then_some(cut)
}

/// Each user turn carries a runtime-context preamble; only the prompt after it
/// is worth handing to the summarizer.
fn user_prompt_only(text: &str) -> &str {
    text.rsplit("\n\n---\n\n").next().unwrap_or(text)
}

fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &text[..end], text.len())
}

fn render(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    let body = if message.role == "user" {
                        user_prompt_only(text)
                    } else {
                        text
                    };
                    if !body.trim().is_empty() {
                        out.push_str(&format!("{}: {}\n", message.role, body.trim()));
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    out.push_str(&format!(
                        "assistant called {name}({})\n",
                        excerpt(&args, 200)
                    ));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let tag = if *is_error {
                        "tool failed"
                    } else {
                        "tool returned"
                    };
                    out.push_str(&format!("{tag}: {}\n", excerpt(content, RESULT_EXCERPT)));
                }
            }
        }
    }
    out
}

/// Ask the model to summarize `history`. Returns the brief.
pub fn summarize(
    config: &Config,
    history: &[Message],
    cancel: &AtomicBool,
) -> Result<String, String> {
    let transcript = render(history);
    if transcript.trim().is_empty() {
        return Err("nothing substantive to summarize".into());
    }
    let request = vec![Message::user_text(&format!(
        "Compact this session history:\n\n{transcript}"
    ))];
    let summary =
        provider::complete(config, SUMMARY_SYSTEM, &request, cancel).map_err(|e| e.to_string())?;
    if summary.trim().is_empty() {
        return Err("the model returned an empty summary".into());
    }
    Ok(summary.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::user_text(text)
    }

    fn assistant_tool_use() -> Message {
        Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
                signature: None,
            }],
        }
    }

    fn tool_result() -> Message {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "contents".into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn short_sessions_are_left_alone() {
        assert_eq!(plan_cut(&[user("one")]), None);
        assert_eq!(
            plan_cut(&[user("one"), assistant_tool_use(), tool_result()]),
            None
        );
    }

    #[test]
    fn cut_lands_on_a_user_turn_never_on_a_tool_result() {
        let messages = vec![
            user("first"),        // 0
            assistant_tool_use(), // 1
            tool_result(),        // 2
            user("second"),       // 3
            assistant_tool_use(), // 4
            tool_result(),        // 5
            user("third"),        // 6
        ];
        let cut = plan_cut(&messages).expect("a cut");
        assert_eq!(cut, 3, "keeps the last two user turns");
        assert!(
            is_user_turn(&messages[cut]),
            "retained history starts at a real user turn"
        );
    }

    #[test]
    fn rendering_strips_the_runtime_context_preamble() {
        let m = user("# Runtime context\n\n- cwd: /tmp\n\n---\n\nfix the parser");
        let rendered = render(&[m]);
        assert!(rendered.contains("fix the parser"));
        assert!(!rendered.contains("Runtime context"));
    }
}
