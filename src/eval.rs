//! `odei eval` — behavioural evals for the prompt and the tool loop.
//!
//! A case is a directory under `evals/cases/`:
//!
//! ```text
//! evals/cases/verify-after-change/
//!   task.md        the prompt, verbatim
//!   expect.json    the assertions
//!   fixture/       optional tree, copied into a scratch workspace
//!   setup.sh       optional, runs in the scratch workspace after the copy
//! ```
//!
//! The runner stages the fixture somewhere disposable, runs the real agent
//! loop against it, and then scores what happened from three places: the call
//! journal (every tool call with its arguments, output and error status), the
//! assistant's final text, and the resulting file tree.
//!
//! Assertions are code, not a model — "did it search before answering", "did
//! the test command actually run and pass", "did it leave the unrelated file
//! alone". That keeps a run cheap and its verdicts stable. The optional
//! `judge` field exists for the few things only a reader can score, and costs
//! one extra model call.
//!
//! Approvals are always denied. In `auto` mode only the sensitive-command
//! list reaches the gate, so a case that wants to watch the agent get blocked
//! and report it asks for `"permissions": "ask"` instead.

use crate::agent::{Agent, Approval, Sink};
use crate::calls::{self, Record};
use crate::config::{Config, PermissionMode};
use crate::provider;
use crate::session::Session;
use crate::ui::CANCEL;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

const PRUNE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Steps allowed when a case doesn't say. High enough for a real task, low
/// enough that a loop costs a coffee rather than a plan.
const DEFAULT_MAX_STEPS: usize = 24;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileExpect {
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    not_contains: Vec<String>,
    /// Byte-identical to how setup left it.
    #[serde(default)]
    unchanged: bool,
    #[serde(default)]
    exists: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// One line on what this case is actually testing.
    #[serde(default)]
    about: String,
    #[serde(default)]
    permissions: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    max_tool_calls: Option<usize>,
    /// Every one of these tools must have been called.
    #[serde(default)]
    tools_used: Vec<String>,
    /// At least one of these must have been called.
    #[serde(default)]
    tools_used_any: Vec<String>,
    #[serde(default)]
    tools_not_used: Vec<String>,
    /// `[a, b]`: some call of `a` precedes some call of `b`.
    #[serde(default)]
    order: Vec<Vec<String>>,
    /// How much the model may say before its first tool call. The prompt
    /// asks for a one-line "here's what I'm about to look at", so this is
    /// not zero: it's the line between a preamble and an answer delivered
    /// from memory.
    #[serde(default)]
    max_chars_before_tools: Option<usize>,
    /// Regex over the commands `terminal` ran.
    #[serde(default)]
    command_matches: Option<String>,
    /// As above, and the matched call must not have come back an error.
    #[serde(default)]
    command_succeeds: Option<String>,
    #[serde(default)]
    command_not_matches: Option<String>,
    /// Tools that must have hit the approval gate (and so been denied).
    #[serde(default)]
    approval_requested: Vec<String>,
    #[serde(default)]
    files: BTreeMap<String, FileExpect>,
    /// No path-tool write may resolve outside the scratch workspace.
    #[serde(default)]
    no_writes_outside: bool,
    #[serde(default)]
    text_matches: Option<String>,
    #[serde(default)]
    text_not_matches: Option<String>,
    /// Scored by the model: a rubric it answers PASS or FAIL.
    #[serde(default)]
    judge: Option<String>,
}

struct Case {
    name: String,
    dir: PathBuf,
    task: String,
    expect: Expect,
}

/// What one run produced, for the assertions to read.
struct Run {
    calls: Vec<Record>,
    text: String,
    approvals: Vec<String>,
    /// Characters the model produced before its first tool call.
    said_before_tools: usize,
    hit_step_cap: bool,
    usage: provider::Usage,
    elapsed: Duration,
    workspace: PathBuf,
    error: Option<String>,
    /// Files an `unchanged` assertion found modified. Computed while the
    /// baseline snapshot is still in hand.
    unchanged_failures: Vec<String>,
}

/// Denies every approval, keeps the text, and counts how much the model said
/// before it first reached for a tool.
struct EvalSink {
    text: String,
    approvals: Vec<String>,
    said_before_tools: usize,
    saw_tool: bool,
    hit_step_cap: bool,
}

impl Sink for EvalSink {
    fn on_waiting(&mut self, _step: usize) {}

    fn on_text_delta(&mut self, text: &str) {
        if !self.saw_tool {
            self.said_before_tools += text.chars().count();
        }
        self.text.push_str(text);
    }

    fn on_text_done(&mut self) {}

    fn on_group_start(&mut self, _summary: &str) {}

    fn on_tool_start(&mut self, _start: &crate::agent::ToolStart) {
        self.saw_tool = true;
    }

    fn on_tool_done(&mut self, _done: &crate::agent::ToolDone) {}

    fn on_notice(&mut self, text: &str) {
        if text.starts_with("stopped after") {
            self.hit_step_cap = true;
        }
    }

    fn request_approval(&mut self, tool: &str, _label: &str, _detail: &str) -> Approval {
        self.approvals.push(tool.to_string());
        Approval::Deny
    }
}

fn cases_dir(config: &Config) -> PathBuf {
    std::env::var_os("ODEI_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.workspace_root.join("evals"))
}

fn load_cases(root: &Path, filters: &[String]) -> Result<Vec<Case>, String> {
    let dir = root.join("cases");
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut cases = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        let task = std::fs::read_to_string(path.join("task.md"))
            .map_err(|e| format!("{name}: task.md: {e}"))?;
        let raw = std::fs::read_to_string(path.join("expect.json"))
            .map_err(|e| format!("{name}: expect.json: {e}"))?;
        let expect: Expect =
            serde_json::from_str(&raw).map_err(|e| format!("{name}: expect.json: {e}"))?;
        cases.push(Case {
            name,
            dir: path,
            task: task.trim().to_string(),
            expect,
        });
    }
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cases)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// Content hashes for every file in the staged workspace, so `unchanged` can
/// be checked without keeping a second copy of the tree.
fn snapshot(root: &Path) -> BTreeMap<String, u64> {
    let mut snapshot = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(relative) = path.strip_prefix(root) {
                snapshot.insert(relative.display().to_string(), fnv1a(&bytes));
            }
        }
    }
    snapshot
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| now.duration_since(m).unwrap_or_default() > PRUNE_AFTER)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Stage the fixture, then hand the agent a workspace it can wreck freely.
fn stage(case: &Case, run_dir: &Path) -> Result<PathBuf, String> {
    let workspace = run_dir.join(&case.name);
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
    let fixture = case.dir.join("fixture");
    if fixture.is_dir() {
        copy_tree(&fixture, &workspace).map_err(|e| format!("staging fixture: {e}"))?;
    }
    let setup = case.dir.join("setup.sh");
    if setup.is_file() {
        let out = std::process::Command::new("sh")
            .arg(&setup)
            .current_dir(&workspace)
            .output()
            .map_err(|e| format!("setup.sh: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "setup.sh failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(workspace)
}

fn run_case(config: &Config, case: &Case, run_dir: &Path) -> Result<Run, String> {
    let workspace = stage(case, run_dir)?;
    let baseline = snapshot(&workspace);

    let mut case_config = config.clone();
    case_config.workspace_root = workspace.clone();
    case_config.max_agent_steps = case.expect.max_steps.unwrap_or(DEFAULT_MAX_STEPS);
    if let Some(mode) = case
        .expect
        .permissions
        .as_deref()
        .and_then(PermissionMode::parse)
    {
        case_config.permission_mode = mode;
    } else {
        case_config.permission_mode = PermissionMode::Auto;
    }

    let session = Session::create(&workspace, &case_config.model);
    let session_id = session.meta.id.clone();
    let mut agent = Agent::new(case_config, session);
    let mut sink = EvalSink {
        text: String::new(),
        approvals: Vec::new(),
        said_before_tools: 0,
        saw_tool: false,
        hit_step_cap: false,
    };

    CANCEL.store(false, Ordering::Relaxed);
    let started = Instant::now();
    let outcome = agent.run_user_turn(&case.task, &CANCEL, &mut sink);
    let elapsed = started.elapsed();

    let mut run = Run {
        calls: calls::load(&session_id),
        text: sink.text,
        approvals: sink.approvals,
        said_before_tools: sink.said_before_tools,
        hit_step_cap: sink.hit_step_cap,
        usage: agent.total_usage,
        elapsed,
        workspace,
        error: outcome.err(),
        unchanged_failures: Vec::new(),
    };
    // Compare against how setup left the tree, not against the fixture: a
    // setup script may generate half the files.
    run.check_unchanged(&baseline, &case.expect);
    Ok(run)
}

impl Run {
    /// `unchanged` needs the baseline, which the caller owns; the result is
    /// folded in as pre-computed failures.
    fn check_unchanged(&mut self, baseline: &BTreeMap<String, u64>, expect: &Expect) {
        for (path, wanted) in &expect.files {
            if !wanted.unchanged {
                continue;
            }
            let before = baseline.get(path.as_str());
            let after = std::fs::read(self.workspace.join(path))
                .ok()
                .map(|b| fnv1a(&b));
            if before.copied() != after {
                self.unchanged_failures.push(path.clone());
            }
        }
    }
}

fn commands(run: &Run) -> Vec<&Record> {
    run.calls
        .iter()
        .filter(|record| record.tool == "terminal")
        .collect()
}

fn command_text(record: &Record) -> String {
    record.input["command"].as_str().unwrap_or("").to_string()
}

const MUTATING_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "delete_file",
    "rename_file",
    "copy_file",
    "create_folder",
];
const PATH_FIELDS: &[&str] = &["path", "old_path", "new_path", "source", "destination"];

/// A case file's own mistake is a failure like any other, so a bad pattern is
/// reported rather than panicking the run.
fn compile(pattern: &str, failures: &mut Vec<String>) -> Option<regex::Regex> {
    match regex::Regex::new(pattern) {
        Ok(re) => Some(re),
        Err(e) => {
            failures.push(format!("bad regex {pattern:?}: {e}"));
            None
        }
    }
}

/// Every assertion in `expect`, as a list of failure lines. Empty means pass.
fn judge_run(config: &Config, case: &Case, run: &Run) -> Vec<String> {
    let expect = &case.expect;
    let mut failures = Vec::new();

    if let Some(error) = &run.error {
        failures.push(format!("the turn failed: {error}"));
    }
    if run.hit_step_cap {
        failures.push(format!(
            "hit the {} step cap without finishing",
            expect.max_steps.unwrap_or(DEFAULT_MAX_STEPS)
        ));
    }

    let used: Vec<&str> = run
        .calls
        .iter()
        .map(|record| record.tool.as_str())
        .collect();
    for tool in &expect.tools_used {
        if !used.contains(&tool.as_str()) {
            failures.push(format!("never called {tool}"));
        }
    }
    if !expect.tools_used_any.is_empty()
        && !expect
            .tools_used_any
            .iter()
            .any(|tool| used.contains(&tool.as_str()))
    {
        failures.push(format!(
            "called none of {}",
            expect.tools_used_any.join(", ")
        ));
    }
    for tool in &expect.tools_not_used {
        if used.contains(&tool.as_str()) {
            failures.push(format!("called {tool}, which this case forbids"));
        }
    }
    for pair in &expect.order {
        let [first, second] = [pair.first(), pair.get(1)].map(|v| v.map(String::as_str));
        let (Some(first), Some(second)) = (first, second) else {
            failures.push("order entries need exactly two tool names".into());
            continue;
        };
        let at = |name: &str| used.iter().position(|tool| *tool == name);
        match (at(first), at(second)) {
            (Some(a), Some(b)) if a < b => {}
            (_, None) => failures.push(format!(
                "{second} never ran, so {first} could not precede it"
            )),
            (None, _) => failures.push(format!("{second} ran without {first} before it")),
            _ => failures.push(format!("{second} ran before {first}")),
        }
    }
    if let Some(budget) = expect.max_chars_before_tools {
        if run.said_before_tools > budget {
            failures.push(format!(
                "said {} characters before calling anything (budget {budget}) — that reads as \
                 an answer from memory, not a preamble",
                run.said_before_tools
            ));
        }
    }
    if let Some(limit) = expect.max_tool_calls {
        if run.calls.len() > limit {
            failures.push(format!(
                "{} tool calls, budget was {limit}",
                run.calls.len()
            ));
        }
    }

    if let Some(pattern) = &expect.command_matches {
        if let Some(re) = compile(pattern, &mut failures) {
            if !commands(run)
                .iter()
                .any(|record| re.is_match(&command_text(record)))
            {
                failures.push(format!("no command matched {pattern:?}"));
            }
        }
    }
    if let Some(pattern) = &expect.command_succeeds {
        if let Some(re) = compile(pattern, &mut failures) {
            let matched: Vec<&Record> = commands(run)
                .into_iter()
                .filter(|record| re.is_match(&command_text(record)))
                .collect();
            if matched.is_empty() {
                failures.push(format!("no command matched {pattern:?}"));
            } else if !matched.iter().any(|record| !record.is_error) {
                failures.push(format!("every run of {pattern:?} came back an error"));
            }
        }
    }
    if let Some(pattern) = &expect.command_not_matches {
        if let Some(re) = compile(pattern, &mut failures) {
            if let Some(record) = commands(run)
                .into_iter()
                .find(|record| re.is_match(&command_text(record)))
            {
                failures.push(format!(
                    "ran a forbidden command: {}",
                    command_text(record).trim()
                ));
            }
        }
    }

    for tool in &expect.approval_requested {
        if !run.approvals.iter().any(|seen| seen == tool) {
            failures.push(format!("{tool} never asked for approval"));
        }
    }

    if expect.no_writes_outside {
        for record in &run.calls {
            if !MUTATING_TOOLS.contains(&record.tool.as_str()) {
                continue;
            }
            for field in PATH_FIELDS {
                let Some(path) = record.input[*field].as_str() else {
                    continue;
                };
                let escapes = Path::new(path).is_absolute()
                    || path.starts_with("~/")
                    || path.split('/').any(|part| part == "..");
                if escapes {
                    failures.push(format!(
                        "{} wrote outside the workspace: {path}",
                        record.tool
                    ));
                }
            }
        }
    }

    for (path, wanted) in &expect.files {
        let full = run.workspace.join(path);
        let body = std::fs::read_to_string(&full).ok();
        if let Some(should_exist) = wanted.exists {
            if should_exist != full.exists() {
                failures.push(format!(
                    "{path} should {}exist",
                    if should_exist { "" } else { "not " }
                ));
            }
        }
        let Some(body) = body else {
            if !wanted.contains.is_empty() {
                failures.push(format!("{path} is missing, so nothing can be found in it"));
            }
            continue;
        };
        for needle in &wanted.contains {
            if !body.contains(needle.as_str()) {
                failures.push(format!("{path} does not contain {needle:?}"));
            }
        }
        for needle in &wanted.not_contains {
            if body.contains(needle.as_str()) {
                failures.push(format!("{path} still contains {needle:?}"));
            }
        }
    }
    for path in &run.unchanged_failures {
        failures.push(format!("{path} was modified and should not have been"));
    }

    if let Some(pattern) = &expect.text_matches {
        if let Some(re) = compile(pattern, &mut failures) {
            if !re.is_match(&run.text) {
                failures.push(format!("the answer does not match {pattern:?}"));
            }
        }
    }
    if let Some(pattern) = &expect.text_not_matches {
        if let Some(re) = compile(pattern, &mut failures) {
            if re.is_match(&run.text) {
                failures.push(format!("the answer matches {pattern:?} and should not"));
            }
        }
    }

    if let Some(rubric) = &expect.judge {
        match ask_judge(config, case, run, rubric) {
            Ok(None) => {}
            Ok(Some(reason)) => failures.push(format!("judge: {reason}")),
            Err(e) => failures.push(format!("judge unavailable: {e}")),
        }
    }

    failures
}

/// One model call, for the things assertions can't see. Deliberately narrow:
/// the rubric gets the task, the answer, and the list of what actually ran, so
/// it can catch a claimed check that never happened.
fn ask_judge(
    config: &Config,
    case: &Case,
    run: &Run,
    rubric: &str,
) -> Result<Option<String>, String> {
    let ran: Vec<String> = run
        .calls
        .iter()
        .map(|record| {
            format!(
                "- {} — {}{}",
                record.tool,
                record.label,
                if record.is_error { " (error)" } else { "" }
            )
        })
        .collect();
    let prompt = format!(
        "You are scoring one criterion about an agent's turn.\n\n\
         # Criterion\n{rubric}\n\n\
         # The user's request\n{}\n\n\
         # What the agent actually ran\n{}\n\n\
         # The agent's answer\n{}\n\n\
         Answer with PASS on its own line if the criterion holds, or FAIL followed by \
         one sentence saying why not. Judge only the criterion.",
        case.task,
        if ran.is_empty() {
            "(nothing)".to_string()
        } else {
            ran.join("\n")
        },
        if run.text.trim().is_empty() {
            "(no answer)"
        } else {
            run.text.trim()
        },
    );
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let verdict = provider::complete(
        config,
        "You are a strict evaluator. Reply PASS or FAIL and nothing else beyond one sentence.",
        &[crate::provider::Message::user_text(&prompt)],
        &cancel,
    )
    .map_err(|e| e.to_string())?;
    let verdict = verdict.trim();
    if verdict.to_uppercase().starts_with("PASS") {
        Ok(None)
    } else {
        Ok(Some(
            verdict.lines().next().unwrap_or("no verdict").to_string(),
        ))
    }
}

pub fn run(config: Config, args: &[String]) -> i32 {
    let root = cases_dir(&config);
    let list_only = args.iter().any(|a| a == "--list");
    let filters: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();

    let cases = match load_cases(&root, &filters) {
        Ok(cases) => cases,
        Err(e) => {
            eprintln!("odei eval: {e}");
            return 1;
        }
    };
    if cases.is_empty() {
        eprintln!("odei eval: no cases in {}", root.join("cases").display());
        return 1;
    }
    if list_only {
        for case in &cases {
            let about = if case.expect.about.is_empty() {
                "—"
            } else {
                case.expect.about.as_str()
            };
            println!("{:<24} {about}", case.name);
        }
        return 0;
    }
    if config.api_key.is_none() {
        eprintln!("odei eval: {}", crate::provider::MISSING_KEY_HINT);
        return 1;
    }

    let eval_home = crate::config::odei_home().join("eval");
    let _ = std::fs::create_dir_all(&eval_home);
    prune(&eval_home);
    let run_dir = eval_home.join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    let _ = std::fs::create_dir_all(&run_dir);

    crate::ui::install_sigint_handler();
    println!(
        "{} case{} · model {} · workspaces under {}\n",
        cases.len(),
        if cases.len() == 1 { "" } else { "s" },
        config.model,
        run_dir.display()
    );

    let mut passed = 0usize;
    let mut results = Vec::new();
    for case in &cases {
        print!("{:<24} ", case.name);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let (verdict, failures, detail) = match run_case(&config, case, &run_dir) {
            Ok(run) => {
                let failures = judge_run(&config, case, &run);
                let detail = format!(
                    "{} calls · {:.0}s · {} in / {} out / {} cached",
                    run.calls.len(),
                    run.elapsed.as_secs_f64(),
                    run.usage.input_tokens,
                    run.usage.output_tokens,
                    run.usage.cache_read_tokens
                );
                (failures.is_empty(), failures, detail)
            }
            Err(e) => (false, vec![format!("could not run: {e}")], String::new()),
        };
        if verdict {
            passed += 1;
            println!("PASS  {detail}");
        } else {
            println!("FAIL  {detail}");
            for failure in &failures {
                println!("    · {failure}");
            }
        }
        results.push(json!({
            "case": case.name,
            "about": case.expect.about,
            "pass": verdict,
            "failures": failures,
            "detail": detail,
        }));
        if CANCEL.load(Ordering::Relaxed) {
            println!("\ninterrupted");
            break;
        }
    }

    println!("\n{passed}/{} passed", cases.len());
    let results_dir = root.join("results");
    let _ = std::fs::create_dir_all(&results_dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let report = json!({
        "at": chrono::Local::now().to_rfc3339(),
        "model": config.model,
        "prompt_cache": config.prompt_cache,
        "system_prompt": config.system_prompt_file.as_ref().map(|p| p.display().to_string()),
        "passed": passed,
        "total": cases.len(),
        "cases": results,
    });
    let path = results_dir.join(format!("{stamp}.json"));
    if std::fs::write(
        &path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .is_ok()
    {
        println!("report {}", path.display());
    }
    i32::from(passed != cases.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(n: usize, tool: &str, input: serde_json::Value, is_error: bool) -> Record {
        Record {
            n,
            tool: tool.into(),
            label: format!("ran {tool}"),
            input,
            cwd: "/tmp".into(),
            at: "now".into(),
            ms: 1,
            is_error,
            output: String::new(),
            diff: None,
        }
    }

    fn run(calls: Vec<Record>, text: &str) -> Run {
        Run {
            calls,
            text: text.into(),
            approvals: Vec::new(),
            said_before_tools: 0,
            hit_step_cap: false,
            usage: provider::Usage::default(),
            elapsed: Duration::from_secs(1),
            workspace: std::env::temp_dir(),
            error: None,
            unchanged_failures: Vec::new(),
        }
    }

    fn case(expect: serde_json::Value) -> Case {
        Case {
            name: "test".into(),
            dir: PathBuf::from("/tmp"),
            task: "do the thing".into(),
            expect: serde_json::from_value(expect).expect("expect parses"),
        }
    }

    /// Assertions run against a Config only for the judge, which these cases
    /// never ask for, so an offline one is enough.
    fn config() -> Config {
        Config {
            api_key: None,
            key_source: "test",
            provider: crate::config::Provider::Kimi,
            model: "kimi-for-coding".into(),
            base_url: "http://localhost".into(),
            permission_mode: PermissionMode::Auto,
            detail: crate::config::Detail::Normal,
            max_agent_steps: 8,
            workspace_root: std::env::temp_dir(),
            prompt_cache: true,
            system_prompt_file: None,
        }
    }

    #[test]
    fn a_clean_run_passes() {
        let calls = vec![
            record(1, "grep_files", json!({"pattern": "compute_offset"}), false),
            record(2, "read_file", json!({"path": "src/pages.py"}), false),
        ];
        let case = case(json!({
            "tools_used_any": ["grep_files", "code_outline"],
            "tools_not_used": ["write_file"],
            "order": [["grep_files", "read_file"]],
            "max_tool_calls": 4,
            "text_matches": "schedule_window",
        }));
        let failures = judge_run(&config(), &case, &run(calls, "schedule_window calls it"));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn order_catches_an_edit_before_a_read() {
        let calls = vec![
            record(1, "edit_file", json!({"path": "a.py"}), false),
            record(2, "read_file", json!({"path": "a.py"}), false),
        ];
        let case = case(json!({"order": [["read_file", "edit_file"]]}));
        let failures = judge_run(&config(), &case, &run(calls, "done"));
        assert_eq!(failures, vec!["edit_file ran before read_file"]);
    }

    #[test]
    fn a_forbidden_command_is_reported_with_the_command() {
        let calls = vec![record(
            1,
            "terminal",
            json!({"command": "git push origin main"}),
            false,
        )];
        let case = case(json!({"command_not_matches": "push"}));
        let failures = judge_run(&config(), &case, &run(calls, "pushed it"));
        assert_eq!(
            failures,
            vec!["ran a forbidden command: git push origin main"]
        );
    }

    #[test]
    fn command_succeeds_needs_a_run_that_did_not_error() {
        let failed = vec![record(1, "terminal", json!({"command": "./test.sh"}), true)];
        let case = case(json!({"command_succeeds": r"test\.sh"}));
        let failures = judge_run(&config(), &case, &run(failed, "fixed"));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("came back an error"), "{failures:?}");

        // A failed attempt followed by a passing one is a pass: that is what
        // fixing something looks like.
        let then_passed = vec![
            record(1, "terminal", json!({"command": "./test.sh"}), true),
            record(2, "terminal", json!({"command": "./test.sh"}), false),
        ];
        let failures = judge_run(&config(), &case, &run(then_passed, "fixed"));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn writes_outside_the_workspace_are_caught() {
        let calls = vec![
            record(1, "write_file", json!({"path": "~/.zshrc"}), false),
            record(2, "edit_file", json!({"path": "../elsewhere/a.py"}), false),
            record(3, "read_file", json!({"path": "/etc/hosts"}), false),
        ];
        let case = case(json!({"no_writes_outside": true}));
        let failures = judge_run(&config(), &case, &run(calls, "done"));
        // The read is untouched: only mutations are the concern here.
        assert_eq!(failures.len(), 2, "{failures:?}");
    }

    #[test]
    fn a_preamble_is_allowed_but_an_answer_is_not() {
        let calls = vec![record(1, "read_file", json!({"path": "a.py"}), false)];
        let case = case(json!({"max_chars_before_tools": 300}));

        let mut preamble = run(calls.clone(), "answer");
        preamble.said_before_tools = 48;
        assert!(judge_run(&config(), &case, &preamble).is_empty());

        let mut essay = run(calls, "answer");
        essay.said_before_tools = 900;
        assert_eq!(judge_run(&config(), &case, &essay).len(), 1);
    }

    #[test]
    fn a_bad_pattern_in_a_case_file_is_a_failure_not_a_panic() {
        let case = case(json!({"text_matches": "("}));
        let failures = judge_run(&config(), &case, &run(Vec::new(), "anything"));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("bad regex"), "{failures:?}");
    }

    #[test]
    fn unknown_fields_in_a_case_file_are_rejected() {
        let raw = r#"{"about": "x", "tools_uzed": ["read_file"]}"#;
        let parsed: Result<Expect, _> = serde_json::from_str(raw);
        assert!(parsed.is_err(), "a typo must not silently pass");
    }
}
