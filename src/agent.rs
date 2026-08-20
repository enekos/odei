//! Agent loop: stream a model turn, execute requested tools behind the
//! permission gate, feed results back, repeat until the model ends its turn
//! or the step cap is reached (ODEI_MAX_AGENT_STEPS).

use crate::compact;
use crate::config::Config;
use crate::context;
use crate::permissions::{self, Decision, RuleStore};
use crate::provider::{self, ContentBlock, Message, StreamEvent, Usage};
use crate::session::Session;
use crate::tools::{self, ToolContext};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

pub enum Approval {
    Allow,
    AlwaysAllow,
    Deny,
}

/// UI hooks. The interactive shell renders activity lines and approval
/// screens; `odei ask` prints plain text and denies interactive approvals.
pub trait Sink {
    fn on_waiting(&mut self);
    fn on_text_delta(&mut self, text: &str);
    fn on_text_done(&mut self);
    fn on_group_start(&mut self, summary: &str);
    fn on_tool_start(&mut self, label: &str, last_in_group: bool);
    fn on_tool_done(&mut self, label: &str, is_error: bool, last_in_group: bool);
    fn on_notice(&mut self, text: &str);
    fn request_approval(&mut self, tool: &str, label: &str, detail: &str) -> Approval;
}

pub struct Agent {
    pub config: Config,
    pub tool_context: ToolContext,
    pub rules: RuleStore,
    pub session: Session,
    pub turns: u64,
    pub total_usage: Usage,
    /// Input tokens on the most recent request — the live context size, as
    /// opposed to `total_usage`, which accumulates over the session.
    pub last_input_tokens: u64,
}

impl Agent {
    pub fn new(config: Config, session: Session) -> Agent {
        let tool_context = ToolContext::new(&config.workspace_root);
        let rules = permissions::load_rules();
        Agent {
            config,
            tool_context,
            rules,
            session,
            turns: 0,
            total_usage: Usage::default(),
            last_input_tokens: 0,
        }
    }

    /// Fraction of the model's window the last request occupied.
    pub fn context_fraction(&self) -> f64 {
        if self.last_input_tokens == 0 {
            return 0.0;
        }
        self.last_input_tokens as f64 / self.config.context_window() as f64
    }

    /// Summarize the older turns and splice the brief in. Returns a one-line
    /// report for the caller to surface.
    pub fn compact_now(
        &mut self,
        cancel: &AtomicBool,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        let Some(cut) = compact::plan_cut(&self.session.messages) else {
            return Ok("not enough history yet to be worth compacting".into());
        };
        sink.on_notice(&format!("compacting {cut} earlier messages…"));
        let summary = compact::summarize(&self.config, &self.session.messages[..cut], cancel)?;
        let mut kept = self.session.messages[cut..].to_vec();
        kept[0].content.insert(
            0,
            ContentBlock::Text {
                text: format!("# Earlier in this session, compacted\n\n{summary}\n\n---\n\n"),
            },
        );
        self.session.messages = kept;
        self.session.rewrite();
        // The next response tells us the real figure; until then assume relief.
        self.last_input_tokens = 0;
        Ok(format!("compacted {cut} messages into a {}-word brief", summary.split_whitespace().count()))
    }

    /// Compact automatically once the window is most of the way full.
    fn compact_if_needed(&mut self, cancel: &AtomicBool, sink: &mut dyn Sink) {
        const TRIGGER: f64 = 0.75;
        if self.context_fraction() < TRIGGER {
            return;
        }
        match self.compact_now(cancel, sink) {
            Ok(report) => sink.on_notice(&report),
            // Compaction is an optimisation; a failure must not kill the turn.
            Err(e) => sink.on_notice(&format!("could not compact ({e}); continuing")),
        }
    }

    pub fn run_user_turn(
        &mut self,
        user_text: &str,
        cancel: &AtomicBool,
        sink: &mut dyn Sink,
    ) -> Result<(), String> {
        // Runtime context rides along as part of the user turn.
        let contextualized = format!(
            "{}\n\n---\n\n{}",
            context::runtime_context(&self.config.workspace_root),
            user_text
        );
        self.compact_if_needed(cancel, sink);
        self.session.append(Message::user_text(&contextualized));
        self.turns += 1;

        let system = context::system_prompt();
        let gateway_tools = tools::gateway_tools();

        for _step in 0..self.config.max_agent_steps {
            sink.on_waiting();
            let mut saw_text = false;
            let result = provider::stream_turn(
                &self.config,
                &system,
                &self.session.messages,
                &gateway_tools,
                cancel,
                &mut |event| match event {
                    StreamEvent::TextDelta(piece) => {
                        saw_text = true;
                        sink.on_text_delta(piece);
                    }
                    StreamEvent::ToolUseStart { .. } | StreamEvent::ThinkingDelta(_) => {}
                },
            );

            let turn = match result {
                Ok(turn) => turn,
                Err(provider::ProviderError::Cancelled) => {
                    if saw_text {
                        sink.on_text_done();
                    }
                    sink.on_notice("interrupted");
                    return Ok(());
                }
                Err(e) => {
                    if saw_text {
                        sink.on_text_done();
                    }
                    return Err(e.to_string());
                }
            };
            if saw_text {
                sink.on_text_done();
            }

            self.total_usage.input_tokens += turn.usage.input_tokens;
            self.total_usage.output_tokens += turn.usage.output_tokens;
            if turn.usage.input_tokens > 0 {
                self.last_input_tokens = turn.usage.input_tokens;
            }
            crate::session::record_usage(&self.config.model, turn.usage);

            let tool_calls: Vec<(String, String, Value)> = turn
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            self.session.append(Message { role: "assistant".into(), content: turn.content.clone() });

            if tool_calls.is_empty() || turn.stop_reason != "tool_use" {
                return Ok(());
            }

            let kinds: Vec<tools::ActivityKind> = tool_calls
                .iter()
                .filter_map(|(_, name, _)| tools::find(name).map(|s| s.activity_kind))
                .collect();
            sink.on_group_start(&tools::group_summary(&kinds));

            let mut results: Vec<ContentBlock> = Vec::new();
            let total = tool_calls.len();
            for (index, (id, name, input)) in tool_calls.into_iter().enumerate() {
                let last = index + 1 == total;
                let outcome = self.execute_tool(&name, &input, last, cancel, sink);
                // Oversized results go to the store; the transcript keeps a
                // preview plus a handle the model can page through.
                let content = if outcome.text.len() > tools::results::INLINE_CAP {
                    self.tool_context.results.preview(&outcome.text)
                } else {
                    outcome.text
                };
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    is_error: outcome.is_error,
                });
            }
            self.session.append(Message { role: "user".into(), content: results });
        }

        sink.on_notice(&format!(
            "stopped after {} agent steps (ODEI_MAX_AGENT_STEPS)",
            self.config.max_agent_steps
        ));
        Ok(())
    }

    fn execute_tool(
        &mut self,
        name: &str,
        input: &Value,
        last_in_group: bool,
        cancel: &AtomicBool,
        sink: &mut dyn Sink,
    ) -> tools::ToolOutcome {
        let Some(spec) = tools::find(name) else {
            return tools::ToolOutcome::err(format!("unknown tool: {name}"));
        };
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return tools::ToolOutcome::err("cancelled by user");
        }

        let running_label = tools::activity_label(spec, input, false);
        let decision = permissions::classify(
            &self.tool_context,
            &self.rules,
            self.config.permission_mode,
            spec,
            input,
        );
        if decision == Decision::NeedsApproval {
            let detail = serde_json::to_string_pretty(input).unwrap_or_default();
            match sink.request_approval(spec.name, &running_label, &detail) {
                Approval::Allow => {}
                Approval::AlwaysAllow => {
                    let target = input[spec.label_arg].as_str().unwrap_or("");
                    // Terminal rules remember the first command token.
                    let target = if spec.name == "terminal" {
                        target.split_whitespace().next().unwrap_or("").to_string()
                    } else {
                        target.to_string()
                    };
                    permissions::remember_allow(&mut self.rules, spec.name, &target);
                }
                Approval::Deny => {
                    sink.on_tool_done(
                        &format!("Denied {running_label}"),
                        true,
                        last_in_group,
                    );
                    return tools::ToolOutcome::err(
                        "the user denied permission for this action; do not retry it without new user intent",
                    );
                }
            }
        }

        sink.on_tool_start(&running_label, last_in_group);
        let outcome = (spec.call)(&self.tool_context, input);
        let done_label = tools::activity_label(spec, input, true);
        sink.on_tool_done(&done_label, outcome.is_error, last_in_group);
        outcome
    }
}
