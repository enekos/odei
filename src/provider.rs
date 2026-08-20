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
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug)]
pub struct TurnResult {
    pub content: Vec<ContentBlock>,
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

    let body = json!({
        "model": config.model,
        "max_tokens": 32768,
        "stream": true,
        "system": system,
        "messages": messages,
        "tools": tools,
    });

    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        attempt += 1;
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
                    if let Some(t) = event["message"]["usage"]["input_tokens"].as_u64() {
                        usage.input_tokens = t;
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
                        // thinking blocks stream but are not echoed back
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
                            on_event(StreamEvent::ThinkingDelta(
                                delta["thinking"].as_str().unwrap_or(""),
                            ));
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
    Ok(TurnResult { content, stop_reason, usage })
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
