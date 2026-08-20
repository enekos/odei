//! Kimi client: the Kimi Code subscription endpoint
//! (https://api.kimi.com/coding), speaking the Anthropic-compatible
//! /v1/messages protocol with SSE streaming and tool use.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    /// Prompt tokens the model actually had to read. With caching on, this
    /// excludes anything served from cache.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens written into the cache by this request.
    pub cache_write_tokens: u64,
    /// Prompt tokens served from cache instead of being read again.
    pub cache_read_tokens: u64,
}

impl Usage {
    /// Everything the model saw as prompt, cached or not — the real context
    /// size. `input_tokens` alone undercounts once caching is on, and the
    /// compaction trigger reads this.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens + self.cache_read_tokens
    }
}

#[derive(Debug)]
pub struct TurnResult {
    pub content: Vec<ContentBlock>,
    /// Reasoning text, kept out of `content` because it is not echoed back to
    /// the API. Every Kimi response opens with a thinking block, and a turn
    /// that produces nothing else leaves this as the only thing the model
    /// actually said.
    pub thinking: String,
    pub stop_reason: String,
    pub usage: Usage,
}

#[allow(dead_code)]
pub enum StreamEvent<'a> {
    TextDelta(&'a str),
    ToolUseStart { name: &'a str },
    ThinkingDelta(&'a str),
}

#[derive(Debug)]
pub enum ProviderError {
    MissingKey,
    Cancelled,
    Http(u16, String),
    Transport(String),
    Protocol(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::MissingKey => {
                write!(f, "no API key configured; run `odei setup` or set KIMI_API_KEY")
            }
            ProviderError::Cancelled => write!(f, "cancelled"),
            ProviderError::Http(status, body) => {
                write!(f, "kimi request failed (HTTP {status}): {body}")
            }
            ProviderError::Transport(msg) => write!(f, "network failure: {msg}"),
            ProviderError::Protocol(msg) => write!(f, "unexpected kimi response: {msg}"),
        }
    }
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Set when the endpoint rejects `cache_control`, so the rest of the process
/// stops sending it. Kimi speaks the Anthropic protocol but is a different
/// implementation; a strict validator must degrade to plain requests rather
/// than fail every turn.
static CACHE_REJECTED: AtomicBool = AtomicBool::new(false);

pub fn cache_rejected() -> bool {
    CACHE_REJECTED.load(Ordering::Relaxed)
}

fn mark(value: &mut Value) {
    value["cache_control"] = json!({"type": "ephemeral"});
}

/// The request, with cache breakpoints if they're wanted.
///
/// The prompt prefix is hashed in request order — tools, then system, then
/// messages — so a breakpoint on the *last* tool covers every schema (~5k
/// tokens that never change), and one on the system block covers the prompt.
/// Both are re-sent on every step of every turn, which is what makes them
/// worth caching at all.
///
/// The transcript needs two breakpoints, not one: the newest message so this
/// turn's context is cached for the next step, and the one before it so this
/// step reads back what the last step wrote. Four in total, which is the
/// documented ceiling.
fn build_body(
    config: &Config,
    system: &str,
    messages: &[Message],
    tools: &[Value],
    cache: bool,
) -> Value {
    let mut body = json!({
        "model": config.model,
        "max_tokens": config.max_tokens(),
        "stream": true,
        "system": system,
        "messages": messages,
        "tools": tools,
    });
    if !cache {
        return body;
    }
    body["system"] = json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]);
    if let Some(last) = body["tools"].as_array_mut().and_then(|tools| tools.last_mut()) {
        mark(last);
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        let count = messages.len();
        for index in [count.checked_sub(3), count.checked_sub(1)].into_iter().flatten() {
            if let Some(block) =
                messages[index]["content"].as_array_mut().and_then(|blocks| blocks.last_mut())
            {
                mark(block);
            }
        }
    }
    body
}

/// One streamed model turn. `on_event` receives deltas for live rendering;
/// the accumulated assistant content is returned when the stream ends.
pub fn stream_turn(
    config: &Config,
    system: &str,
    messages: &[Message],
    tools: &[Value],
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<TurnResult, ProviderError> {
    let key = config.api_key.as_deref().ok_or(ProviderError::MissingKey)?;
    let url = format!("{}/v1/messages", config.base_url);

    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        attempt += 1;
        let cache = config.prompt_cache && !cache_rejected();
        let body = build_body(config, system, messages, tools, cache);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(600))
            .build();
        let response = agent
            .post(&url)
            .set("x-api-key", key)
            .set("authorization", &format!("Bearer {key}"))
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .set("accept", "text/event-stream")
            .send_string(&body.to_string());

        match response {
            Ok(resp) => return read_stream(resp.into_reader(), cancel, on_event),
            Err(ureq::Error::Status(status, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                // A rejected breakpoint is our fault, not the caller's: drop
                // caching for the rest of the process and send the same turn
                // again plainly. Costs one wasted request, once.
                if status == 400 && cache && text.contains("cache") {
                    CACHE_REJECTED.store(true, Ordering::Relaxed);
                    attempt -= 1;
                    continue;
                }
                if retryable_status(status) && attempt < 4 {
                    std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
                    continue;
                }
                let brief: String = text.chars().take(400).collect();
                return Err(ProviderError::Http(status, brief));
            }
            Err(ureq::Error::Transport(t)) => {
                if attempt < 4 {
                    std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
                    continue;
                }
                return Err(ProviderError::Transport(t.to_string()));
            }
        }
    }
}

fn read_stream(
    reader: impl Read,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<TurnResult, ProviderError> {
    let mut lines = BufReader::new(reader);
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut thinking = String::new();
    let mut partial_json: Vec<String> = Vec::new();
    let mut stop_reason = String::from("end_turn");
    let mut usage = Usage::default();
    let mut line = String::new();
    let mut data = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        line.clear();
        let n = lines
            .read_line(&mut line)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        } else if trimmed.is_empty() && !data.is_empty() {
            let event: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Protocol(format!("bad SSE payload: {e}")))?;
            data.clear();
            match event["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let reported = &event["message"]["usage"];
                    if let Some(t) = reported["input_tokens"].as_u64() {
                        usage.input_tokens = t;
                    }
                    if let Some(t) = reported["cache_creation_input_tokens"].as_u64() {
                        usage.cache_write_tokens = t;
                    }
                    if let Some(t) = reported["cache_read_input_tokens"].as_u64() {
                        usage.cache_read_tokens = t;
                    }
                }
                "content_block_start" => {
                    let block = &event["content_block"];
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            content.push(ContentBlock::Text { text: String::new() });
                            partial_json.push(String::new());
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or("").to_string();
                            on_event(StreamEvent::ToolUseStart { name: &name });
                            content.push(ContentBlock::ToolUse {
                                id: block["id"].as_str().unwrap_or("").to_string(),
                                name,
                                input: Value::Null,
                            });
                            partial_json.push(String::new());
                        }
                        // A thinking block still occupies an index, so it
                        // gets a placeholder to keep later deltas aligned;
                        // the text goes to `thinking` and the placeholder is
                        // dropped below.
                        _ => {
                            content.push(ContentBlock::Text { text: String::new() });
                            partial_json.push(String::new());
                        }
                    }
                }
                "content_block_delta" => {
                    let index = event["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &event["delta"];
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let piece = delta["text"].as_str().unwrap_or("");
                            on_event(StreamEvent::TextDelta(piece));
                            if let Some(ContentBlock::Text { text }) = content.get_mut(index) {
                                text.push_str(piece);
                            }
                        }
                        "input_json_delta" => {
                            let piece = delta["partial_json"].as_str().unwrap_or("");
                            if let Some(slot) = partial_json.get_mut(index) {
                                slot.push_str(piece);
                            }
                        }
                        "thinking_delta" => {
                            let piece = delta["thinking"].as_str().unwrap_or("");
                            thinking.push_str(piece);
                            on_event(StreamEvent::ThinkingDelta(piece));
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = event["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(ContentBlock::ToolUse { input, .. }) = content.get_mut(index) {
                        let raw = partial_json.get(index).map(String::as_str).unwrap_or("");
                        *input = if raw.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string()))
                        };
                    }
                }
                "message_delta" => {
                    if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                        stop_reason = reason.to_string();
                    }
                    if let Some(t) = event["usage"]["output_tokens"].as_u64() {
                        usage.output_tokens = t;
                    }
                }
                "message_stop" => break,
                "error" => {
                    let msg = event["error"]["message"].as_str().unwrap_or("unknown");
                    return Err(ProviderError::Protocol(format!("stream error: {msg}")));
                }
                _ => {}
            }
        }
    }

    // Drop empty text blocks the protocol sometimes emits around tool use.
    content.retain(|block| !matches!(block, ContentBlock::Text { text } if text.is_empty()));
    Ok(TurnResult { content, thinking, stop_reason, usage })
}

/// One turn with no tools and no streaming surface, for internal work like
/// summarizing a transcript. Returns the concatenated text.
pub fn complete(
    config: &Config,
    system: &str,
    messages: &[Message],
    cancel: &AtomicBool,
) -> Result<String, ProviderError> {
    let turn = stream_turn(config, system, messages, &[], cancel, &mut |_| {})?;
    Ok(turn
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Non-streaming utility request used by `odei doctor` for connectivity checks.
pub fn check_connectivity(config: &Config) -> Result<(), ProviderError> {
    let key = config.api_key.as_deref().ok_or(ProviderError::MissingKey)?;
    let url = format!("{}/v1/messages", config.base_url);
    let body = json!({
        "model": config.model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let resp = ureq::post(&url)
        .set("x-api-key", key)
        .set("authorization", &format!("Bearer {key}"))
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(30))
        .send_string(&body.to_string());
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(ProviderError::Http(status, text.chars().take(300).collect()))
        }
        Err(ureq::Error::Transport(t)) => Err(ProviderError::Transport(t.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionMode;

    fn config() -> Config {
        Config {
            api_key: Some("test".into()),
            key_source: "test",
            model: "kimi-for-coding".into(),
            base_url: "http://localhost".into(),
            permission_mode: PermissionMode::Auto,
            max_agent_steps: 10,
            workspace_root: std::path::PathBuf::from("/tmp"),
            prompt_cache: true,
            system_prompt_file: None,
        }
    }

    fn transcript(turns: usize) -> Vec<Message> {
        (0..turns)
            .map(|i| Message {
                role: if i % 2 == 0 { "user".into() } else { "assistant".into() },
                content: vec![ContentBlock::Text { text: format!("message {i}") }],
            })
            .collect()
    }

    fn breakpoints(body: &Value) -> usize {
        fn walk(value: &Value) -> usize {
            match value {
                Value::Object(map) => {
                    map.contains_key("cache_control") as usize
                        + map.values().map(walk).sum::<usize>()
                }
                Value::Array(items) => items.iter().map(walk).sum(),
                _ => 0,
            }
        }
        walk(body)
    }

    #[test]
    fn uncached_body_is_untouched() {
        let body = build_body(&config(), "be helpful", &transcript(4), &[json!({"name": "a"})], false);
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["max_tokens"], 32768);
        assert_eq!(breakpoints(&body), 0);
    }

    #[test]
    fn cached_body_marks_tools_system_and_the_transcript_tail() {
        let tools = vec![json!({"name": "a"}), json!({"name": "b"})];
        let body = build_body(&config(), "be helpful", &transcript(6), &tools, true);
        // System becomes a block array so it can carry a breakpoint.
        assert_eq!(body["system"][0]["text"], "be helpful");
        assert!(body["system"][0]["cache_control"].is_object());
        // Only the last tool: the breakpoint covers the whole schema prefix.
        assert!(body["tools"][0]["cache_control"].is_null());
        assert!(body["tools"][1]["cache_control"].is_object());
        // Newest message, so the next step reads back this turn's context,
        // and the one before it, so this step reads back the last step's.
        assert!(body["messages"][5]["content"][0]["cache_control"].is_object());
        assert!(body["messages"][3]["content"][0]["cache_control"].is_object());
        assert!(body["messages"][4]["content"][0]["cache_control"].is_null());
        // Four is the documented ceiling; going over is a hard API error.
        assert_eq!(breakpoints(&body), 4);
    }

    #[test]
    fn a_short_transcript_marks_what_exists() {
        let body = build_body(&config(), "be helpful", &transcript(1), &[json!({"name": "a"})], true);
        assert_eq!(breakpoints(&body), 3);
        assert!(body["messages"][0]["content"][0]["cache_control"].is_object());
    }

    #[test]
    fn context_tokens_counts_cached_prompt() {
        let usage = Usage {
            input_tokens: 300,
            output_tokens: 50,
            cache_write_tokens: 1_200,
            cache_read_tokens: 40_000,
        };
        // The window holds 41,500 prompt tokens even though the model only
        // had to read 300 of them.
        assert_eq!(usage.context_tokens(), 41_500);
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    /// Build an SSE body the way the endpoint sends one: `data:` lines
    /// separated by blank lines.
    fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("data: {e}\n\n")).collect()
    }

    fn read(body: &str) -> TurnResult {
        let cancel = AtomicBool::new(false);
        read_stream(body.as_bytes(), &cancel, &mut |_| {}).expect("stream parses")
    }

    fn thinking_block(index: u64, text: &str) -> Vec<Value> {
        vec![
            json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking"}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":text}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":index}),
        ]
    }

    #[test]
    fn thinking_is_captured_and_kept_out_of_the_content() {
        let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})];
        events.extend(thinking_block(0, "the file is small, I will read it"));
        events.extend([
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"It greets."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ]);
        let turn = read(&sse(&events));
        // The thinking block held index 0, so the text must not have been
        // appended into it — one block out, and it is the answer.
        assert_eq!(turn.content.len(), 1);
        assert!(matches!(&turn.content[0], ContentBlock::Text { text } if text == "It greets."));
        assert_eq!(turn.thinking, "the file is small, I will read it");
    }

    #[test]
    fn a_tool_call_survives_an_end_turn_stop_reason() {
        // Kimi pairs tool_use blocks with stop_reason "end_turn". The loop
        // used to take that as "the turn is over" and drop the call.
        let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})];
        events.extend(thinking_block(0, "I should look at the file"));
        events.extend([
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"app.py\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]);
        let turn = read(&sse(&events));
        assert_eq!(turn.stop_reason, "end_turn");
        match &turn.content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "app.py");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn a_thinking_only_turn_returns_no_content_but_keeps_the_thought() {
        let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})];
        events.extend(thinking_block(0, "I have already answered this"));
        events.extend([
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ]);
        let turn = read(&sse(&events));
        assert!(turn.content.is_empty(), "the placeholder must not survive");
        assert_eq!(turn.thinking, "I have already answered this");
    }

    #[test]
    fn cache_counters_come_off_message_start() {
        let events = [
            json!({"type":"message_start","message":{"usage":{
                "input_tokens": 39, "cache_read_input_tokens": 4416,
                "cache_creation_input_tokens": 0}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let turn = read(&sse(&events));
        assert_eq!(turn.usage.cache_read_tokens, 4416);
        assert_eq!(turn.usage.input_tokens, 39);
        // 39 read fresh, 4416 from cache: the window is holding 4455.
        assert_eq!(turn.usage.context_tokens(), 4455);
    }
}
