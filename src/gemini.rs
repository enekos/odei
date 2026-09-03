//! Gemini client: the Google AI Studio API
//! (https://generativelanguage.googleapis.com), speaking v1beta
//! streamGenerateContent with SSE streaming and function calling. The rest of
//! odei thinks in the Anthropic shapes from `provider`; this module translates
//! at the boundary in both directions.

use crate::config::Config;
use crate::provider::{ContentBlock, Message, ProviderError, StreamEvent, TurnResult, Usage};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Gemini function calls carry no id, but the transcript and the journal need
/// one; a process-wide counter keeps synthesized ids unique across turns.
static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// Function declarations take an OpenAPI subset; the keys odei's schemas use
/// all fit, and anything else is dropped rather than rejected server-side.
fn sanitize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if key == "additionalProperties" || key == "$schema" {
                    continue;
                }
                out.insert(key.clone(), sanitize_schema(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

fn declarations(tools: &[Value]) -> Value {
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool["name"],
                "description": tool["description"],
                "parameters": sanitize_schema(&tool["input_schema"]),
            })
        })
        .collect();
    json!([{ "functionDeclarations": declarations }])
}

/// The transcript, translated. A functionResponse is matched to its call by
/// tool *name*, not id, so the names are collected from every tool_use block
/// first — that also covers ids minted by the Kimi path in a resumed session.
fn contents(messages: &[Message]) -> Vec<Value> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let mut out = Vec::new();
    for message in messages {
        let role = if message.role == "assistant" {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                ContentBlock::ToolUse {
                    name,
                    input,
                    signature,
                    ..
                } => {
                    let args = if input.is_object() {
                        input.clone()
                    } else {
                        json!({})
                    };
                    let mut part = json!({ "functionCall": { "name": name, "args": args } });
                    // Gemini 3 requires the thought signature echoed on the
                    // part, or the continuation is rejected with HTTP 400.
                    if let Some(signature) = signature {
                        part["thoughtSignature"] = json!(signature);
                    }
                    parts.push(part);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let name = names.get(tool_use_id.as_str()).copied().unwrap_or("tool");
                    let response = if *is_error {
                        json!({ "error": content })
                    } else {
                        json!({ "result": content })
                    };
                    parts.push(json!({
                        "functionResponse": { "name": name, "response": response }
                    }));
                }
            }
        }
        if !parts.is_empty() {
            out.push(json!({ "role": role, "parts": parts }));
        }
    }
    out
}

fn build_body(config: &Config, system: &str, messages: &[Message], tools: &[Value]) -> Value {
    let mut body = json!({
        "systemInstruction": { "parts": [{ "text": system }] },
        "contents": contents(messages),
        "generationConfig": { "maxOutputTokens": config.max_tokens() },
    });
    if !tools.is_empty() {
        body["tools"] = declarations(tools);
    }
    body
}

pub fn stream_turn(
    config: &Config,
    system: &str,
    messages: &[Message],
    tools: &[Value],
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<TurnResult, ProviderError> {
    let key = config.api_key.as_deref().ok_or(ProviderError::MissingKey)?;
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        config.base_url, config.model
    );

    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        attempt += 1;
        let body = build_body(config, system, messages, tools);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(600))
            .build();
        let response = agent
            .post(&url)
            .set("x-goog-api-key", key)
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

#[derive(Default)]
struct StreamState {
    text: String,
    thinking: String,
    calls: Vec<ContentBlock>,
    finish: String,
    usage: Usage,
}

fn apply_chunk(
    chunk: &Value,
    state: &mut StreamState,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<(), ProviderError> {
    if chunk["error"].is_object() {
        let msg = chunk["error"]["message"].as_str().unwrap_or("unknown");
        return Err(ProviderError::Protocol(format!("stream error: {msg}")));
    }
    let candidate = &chunk["candidates"][0];
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if part["thought"].as_bool().unwrap_or(false) {
                let piece = part["text"].as_str().unwrap_or("");
                state.thinking.push_str(piece);
                on_event(StreamEvent::ThinkingDelta(piece));
            } else if let Some(piece) = part["text"].as_str() {
                state.text.push_str(piece);
                on_event(StreamEvent::TextDelta(piece));
            } else if part["functionCall"].is_object() {
                let call = &part["functionCall"];
                let name = call["name"].as_str().unwrap_or("").to_string();
                on_event(StreamEvent::ToolUseStart { name: &name });
                let id = format!("{name}-{}", CALL_COUNTER.fetch_add(1, Ordering::Relaxed));
                let args = call["args"].clone();
                state.calls.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input: if args.is_null() { json!({}) } else { args },
                    signature: part["thoughtSignature"].as_str().map(str::to_string),
                });
            }
        }
    }
    if let Some(reason) = candidate["finishReason"].as_str() {
        state.finish = reason.to_string();
    }
    // Usage arrives cumulatively on the chunks; the last one wins.
    let meta = &chunk["usageMetadata"];
    if meta.is_object() {
        let prompt = meta["promptTokenCount"].as_u64().unwrap_or(0);
        let cached = meta["cachedContentTokenCount"].as_u64().unwrap_or(0);
        state.usage.cache_read_tokens = cached;
        state.usage.input_tokens = prompt.saturating_sub(cached);
        state.usage.output_tokens = meta["candidatesTokenCount"].as_u64().unwrap_or(0)
            + meta["thoughtsTokenCount"].as_u64().unwrap_or(0);
    }
    Ok(())
}

fn read_stream(
    reader: impl Read,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<TurnResult, ProviderError> {
    let mut lines = BufReader::new(reader);
    let mut state = StreamState::default();
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
        let at_end = n == 0;
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        } else if (trimmed.is_empty() || at_end) && !data.is_empty() {
            let chunk: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Protocol(format!("bad SSE payload: {e}")))?;
            data.clear();
            apply_chunk(&chunk, &mut state, on_event)?;
        }
        if at_end {
            break;
        }
    }

    let StreamState {
        text,
        thinking,
        calls,
        finish,
        usage,
    } = state;
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    let has_calls = !calls.is_empty();
    content.extend(calls);
    let stop_reason = match finish.as_str() {
        "MAX_TOKENS" => "max_tokens".to_string(),
        "" | "STOP" => if has_calls { "tool_use" } else { "end_turn" }.to_string(),
        other => other.to_ascii_lowercase(),
    };
    Ok(TurnResult {
        content,
        thinking,
        stop_reason,
        usage,
    })
}

pub fn check_connectivity(config: &Config) -> Result<(), ProviderError> {
    let key = config.api_key.as_deref().ok_or(ProviderError::MissingKey)?;
    let url = format!("{}/v1beta/models/{}", config.base_url, config.model);
    let resp = ureq::get(&url)
        .set("x-goog-api-key", key)
        .timeout(Duration::from_secs(30))
        .call();
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(ProviderError::Http(
                status,
                text.chars().take(300).collect(),
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(ProviderError::Transport(t.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PermissionMode, Provider};

    fn config() -> Config {
        Config {
            api_key: Some("test".into()),
            key_source: "test",
            provider: Provider::Gemini,
            model: "gemini-2.5-flash".into(),
            base_url: "http://localhost".into(),
            permission_mode: PermissionMode::Auto,
            detail: crate::config::Detail::Normal,
            max_agent_steps: 10,
            workspace_root: std::path::PathBuf::from("/tmp"),
            prompt_cache: true,
            system_prompt_file: None,
        }
    }

    #[test]
    fn the_transcript_translates_to_contents() {
        let messages = vec![
            Message::user_text("read the config"),
            Message {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "Reading it.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "read_file-0".into(),
                        name: "read_file".into(),
                        input: json!({"path": "config.rs"}),
                        signature: Some("sig-abc".into()),
                    },
                ],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "read_file-0".into(),
                    content: "pub struct Config;".into(),
                    is_error: false,
                }],
            },
        ];
        let contents = contents(&messages);
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "Reading it.");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "read_file");
        assert_eq!(
            contents[1]["parts"][1]["functionCall"]["args"]["path"],
            "config.rs"
        );
        // The captured thought signature rides the same part on replay.
        assert_eq!(contents[1]["parts"][1]["thoughtSignature"], "sig-abc");
        // The response is matched back to its call by name, resolved via the id.
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            "pub struct Config;"
        );
    }

    #[test]
    fn an_error_result_becomes_an_error_response() {
        let messages = vec![
            Message {
                role: "assistant".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "terminal-3".into(),
                    name: "terminal".into(),
                    input: json!({"command": "false"}),
                    signature: None,
                }],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "terminal-3".into(),
                    content: "exit 1".into(),
                    is_error: true,
                }],
            },
        ];
        let contents = contents(&messages);
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["response"]["error"],
            "exit 1"
        );
        // A signature-less call (Kimi-minted, or a pre-signature session)
        // replays without the field rather than inventing one.
        assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
    }

    #[test]
    fn a_function_call_keeps_its_thought_signature() {
        let events = [json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.py"}},
             "thoughtSignature": "sig-1"},
            {"functionCall": {"name": "read_file", "args": {"path": "b.py"}}}
        ], "role": "model"}, "finishReason": "STOP"}]})];
        let turn = read(&sse(&events));
        let signatures: Vec<Option<&str>> = turn
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolUse { signature, .. } => signature.as_deref(),
                other => panic!("expected a tool call, got {other:?}"),
            })
            .collect();
        assert_eq!(signatures, [Some("sig-1"), None]);
    }

    #[test]
    fn the_body_carries_system_tools_and_the_output_ceiling() {
        let tools = vec![json!({
            "name": "read_file",
            "description": "Read a file.",
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        })];
        let body = build_body(&config(), "be helpful", &[Message::user_text("hi")], &tools);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 65_536);
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["name"], "read_file");
        // Keys outside Gemini's schema subset are stripped, the rest survive.
        assert!(declaration["parameters"]["additionalProperties"].is_null());
        assert_eq!(declaration["parameters"]["required"][0], "path");
    }

    #[test]
    fn a_toolless_body_sends_no_tools_field() {
        let body = build_body(&config(), "sys", &[Message::user_text("hi")], &[]);
        assert!(body.get("tools").is_none());
    }

    fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("data: {e}\n\n")).collect()
    }

    fn read(body: &str) -> TurnResult {
        let cancel = AtomicBool::new(false);
        read_stream(body.as_bytes(), &cancel, &mut |_| {}).expect("stream parses")
    }

    #[test]
    fn text_chunks_accumulate_into_one_block() {
        let events = [
            json!({"candidates": [{"content": {"parts": [{"text": "It "}], "role": "model"}}]}),
            json!({"candidates": [{"content": {"parts": [{"text": "greets."}], "role": "model"},
                "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 4}}),
        ];
        let turn = read(&sse(&events));
        assert_eq!(turn.content.len(), 1);
        assert!(matches!(&turn.content[0], ContentBlock::Text { text } if text == "It greets."));
        assert_eq!(turn.stop_reason, "end_turn");
        assert_eq!(turn.usage.input_tokens, 12);
        assert_eq!(turn.usage.output_tokens, 4);
    }

    #[test]
    fn a_function_call_becomes_a_tool_use_with_a_unique_id() {
        let events = [json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "app.py"}}},
                {"functionCall": {"name": "read_file", "args": {"path": "lib.py"}}}
            ], "role": "model"}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 30, "candidatesTokenCount": 9}})];
        let turn = read(&sse(&events));
        assert_eq!(turn.stop_reason, "tool_use");
        let ids: Vec<&str> = turn
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    assert_eq!(name, "read_file");
                    assert!(input["path"].is_string());
                    id.as_str()
                }
                other => panic!("expected a tool call, got {other:?}"),
            })
            .collect();
        assert_ne!(ids[0], ids[1], "synthesized ids must not collide");
    }

    #[test]
    fn thought_parts_stay_out_of_the_content() {
        let events = [json!({"candidates": [{"content": {"parts": [
                {"text": "the file is small", "thought": true},
                {"text": "It greets."}
            ], "role": "model"}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4,
                    "thoughtsTokenCount": 7}})];
        let turn = read(&sse(&events));
        assert_eq!(turn.content.len(), 1);
        assert!(matches!(&turn.content[0], ContentBlock::Text { text } if text == "It greets."));
        assert_eq!(turn.thinking, "the file is small");
        assert_eq!(turn.usage.output_tokens, 11);
    }

    #[test]
    fn cached_prompt_tokens_split_out_of_input() {
        let events = [
            json!({"candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"},
                "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 5000, "cachedContentTokenCount": 4500,
                    "candidatesTokenCount": 2}}),
        ];
        let turn = read(&sse(&events));
        assert_eq!(turn.usage.input_tokens, 500);
        assert_eq!(turn.usage.cache_read_tokens, 4500);
        assert_eq!(turn.usage.context_tokens(), 5000);
    }

    #[test]
    fn max_tokens_maps_to_the_shared_stop_reason() {
        let events = [
            json!({"candidates": [{"content": {"parts": [{"text": "truncated"}], "role": "model"},
                "finishReason": "MAX_TOKENS"}]}),
        ];
        assert_eq!(read(&sse(&events)).stop_reason, "max_tokens");
    }

    #[test]
    fn an_error_chunk_fails_the_turn() {
        let events = [json!({"error": {"message": "API key not valid", "code": 400}})];
        let cancel = AtomicBool::new(false);
        let result = read_stream(sse(&events).as_bytes(), &cancel, &mut |_| {});
        assert!(matches!(result, Err(ProviderError::Protocol(msg)) if msg.contains("API key")));
    }
}
