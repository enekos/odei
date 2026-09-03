//! Session persistence at ~/.odei/sessions/<id>.jsonl: a meta line
//! followed by one line per message. Usage accounting appends to
//! ~/.odei/usage.jsonl.

use crate::provider::{Message, Usage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub workspace: String,
    pub created: String,
    pub model: String,
}

pub struct Session {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    path: PathBuf,
}

fn new_id() -> String {
    let now = chrono::Local::now();
    let pid = std::process::id();
    format!("{}-{:04x}", now.format("%Y%m%d-%H%M%S"), pid & 0xffff)
}

impl Session {
    pub fn create(workspace: &std::path::Path, model: &str) -> Session {
        let id = new_id();
        let meta = SessionMeta {
            id: id.clone(),
            title: None,
            workspace: workspace.display().to_string(),
            created: chrono::Local::now().to_rfc3339(),
            model: model.to_string(),
        };
        let dir = crate::config::sessions_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{id}.jsonl"));
        let session = Session {
            meta,
            messages: Vec::new(),
            path,
        };
        session.rewrite();
        session
    }

    pub fn open(id: &str) -> Option<Session> {
        let path = crate::config::sessions_dir().join(format!("{id}.jsonl"));
        let text = std::fs::read_to_string(&path).ok()?;
        let mut lines = text.lines();
        let meta: SessionMeta = serde_json::from_str(lines.next()?).ok()?;
        let mut messages = Vec::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(message) = serde_json::from_str::<Message>(line) {
                messages.push(message);
            }
        }
        Some(Session {
            meta,
            messages,
            path,
        })
    }

    pub fn append(&mut self, message: Message) {
        if let Ok(line) = serde_json::to_string(&message) {
            if let Ok(mut file) = OpenOptions::new().append(true).open(&self.path) {
                let _ = writeln!(file, "{line}");
            }
        }
        self.messages.push(message);
    }

    pub fn rename(&mut self, title: &str) {
        self.meta.title = Some(title.to_string());
        self.rewrite();
    }

    /// Replace on-disk contents with current meta + messages (used after
    /// meta changes or /compact).
    pub fn rewrite(&self) {
        let mut out = String::new();
        if let Ok(meta) = serde_json::to_string(&self.meta) {
            out.push_str(&meta);
            out.push('\n');
        }
        for message in &self.messages {
            if let Ok(line) = serde_json::to_string(message) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        let _ = std::fs::write(&self.path, out);
    }
}

pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub workspace: String,
    pub modified: std::time::SystemTime,
    pub messages: usize,
}

pub fn list() -> Vec<SessionSummary> {
    let dir = crate::config::sessions_dir();
    let mut summaries = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return summaries;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut lines = text.lines();
        let Some(first) = lines.next() else { continue };
        let Ok(meta) = serde_json::from_str::<SessionMeta>(first) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        summaries.push(SessionSummary {
            id: meta.id,
            title: meta.title,
            workspace: meta.workspace,
            modified,
            messages: lines.count(),
        });
    }
    summaries.sort_by_key(|s| std::cmp::Reverse(s.modified));
    summaries
}

pub fn latest_for_workspace(workspace: &std::path::Path) -> Option<String> {
    let workspace = workspace.display().to_string();
    list()
        .into_iter()
        .find(|s| s.workspace == workspace)
        .map(|s| s.id)
}

pub fn record_usage(model: &str, usage: Usage) {
    let path = crate::config::odei_home().join("usage.jsonl");
    let _ = std::fs::create_dir_all(crate::config::odei_home());
    let line = json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "model": model,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
    });
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
