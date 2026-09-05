//! Permission policy. Modes: ask (approve every sensitive tool), auto
//! (routine understood development actions run directly; sensitive actions
//! prompt), yolo (everything runs). Auto uses a conservative static
//! classifier.

use crate::config::PermissionMode;
use crate::tools::{ToolContext, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    NeedsApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub effect: String, // "allow" | "deny"
    pub tool: String,
    /// For terminal rules this is the command prefix; for path tools the path.
    pub target: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RuleStore {
    pub rules: Vec<Rule>,
}

fn rules_path() -> std::path::PathBuf {
    crate::config::odei_home().join("permissions.json")
}

pub fn load_rules() -> RuleStore {
    std::fs::read_to_string(rules_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_rules(store: &RuleStore) {
    let _ = std::fs::create_dir_all(crate::config::odei_home());
    if let Ok(text) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(rules_path(), text);
    }
}

pub fn remember_allow(store: &mut RuleStore, tool: &str, target: &str) -> String {
    let id = format!("rule-{}", store.rules.len() + 1);
    store.rules.push(Rule {
        id: id.clone(),
        effect: "allow".into(),
        tool: tool.into(),
        target: target.into(),
    });
    save_rules(store);
    id
}

fn rule_target(spec: &ToolSpec, input: &Value) -> String {
    input[spec.label_arg].as_str().unwrap_or("").to_string()
}

fn rule_matches(rule: &Rule, tool: &str, target: &str) -> bool {
    if rule.tool != tool {
        return false;
    }
    if rule.target.is_empty() {
        return true;
    }
    if tool == "terminal" {
        command_rule_matches(rule.target.trim(), target.trim())
    } else {
        target == rule.target
    }
}

fn command_rule_matches(rule: &str, command: &str) -> bool {
    let rule_tokens: Vec<&str> = rule.split_whitespace().collect();
    let command_tokens: Vec<&str> = command.split_whitespace().collect();
    if rule_tokens.is_empty() {
        return false;
    }
    if command_is_sensitive(command) {
        return command_is_sensitive(rule) && command_tokens == rule_tokens;
    }
    command_tokens.len() >= rule_tokens.len()
        && command_tokens[..rule_tokens.len()] == rule_tokens[..]
}

pub fn remembered_target(tool: &str, target: &str) -> String {
    if tool != "terminal" {
        return target.to_string();
    }
    let command = target.trim();
    if command_is_sensitive(command) {
        return command.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    let mut tokens = command
        .split_whitespace()
        .skip_while(|t| t.contains('=') && !t.starts_with('-'));
    let mut remembered = Vec::new();
    if let Some(program) = tokens.next() {
        remembered.push(program);
        if let Some(sub) = tokens.next() {
            if !sub.starts_with('-') {
                remembered.push(sub);
            }
        }
    }
    remembered.join(" ")
}

/// Commands that stay sensitive even in auto mode. Conservative on purpose:
/// a blocked action surfaces one approval prompt, never silent failure.
const SENSITIVE_COMMAND_PATTERNS: &[&str] = &[
    "sudo ",
    "rm -rf",
    "rm -fr",
    "rm -r /",
    "git push",
    "git reset --hard",
    "git checkout -- ",
    "git checkout .",
    "git clean",
    "git rebase",
    "git commit --amend",
    "git tag ",
    "git filter-branch",
    "npm publish",
    "cargo publish",
    "pnpm publish",
    "gem push",
    "pip upload",
    "twine upload",
    "shutdown",
    "reboot",
    "mkfs",
    "diskutil erase",
    "dd if=",
    "chmod 777",
    "chown -R",
    "curl | sh",
    "| sh",
    "| bash",
    "kill -9 1",
    "launchctl unload",
    "systemctl stop",
    "drop table",
    "drop database",
    "truncate table",
    "> /dev/",
    "ssh ",
    "scp ",
    "rsync --delete",
    "brew uninstall",
    "apt remove",
    "apt-get remove",
];

fn command_is_sensitive(command: &str) -> bool {
    let lower = command.to_lowercase();
    SENSITIVE_COMMAND_PATTERNS.iter().any(|p| lower.contains(p))
        || lower.trim_start().starts_with("rm ") && lower.contains(" -")
}

pub fn classify(
    ctx: &ToolContext,
    rules: &RuleStore,
    mode: PermissionMode,
    spec: &ToolSpec,
    input: &Value,
) -> Decision {
    if mode == PermissionMode::Yolo {
        return Decision::Allow;
    }

    let target = rule_target(spec, input);
    if rules
        .rules
        .iter()
        .any(|r| r.effect == "allow" && rule_matches(r, spec.name, &target))
    {
        return Decision::Allow;
    }

    if !spec.requires_approval {
        // Reads are free, but external reads prompt in ask mode.
        if mode == PermissionMode::Ask {
            if let Some(path) = input["path"].as_str() {
                let resolved = ctx.resolve(path);
                if ctx.is_external(&resolved) {
                    return Decision::NeedsApproval;
                }
            }
        }
        return Decision::Allow;
    }

    match mode {
        PermissionMode::Ask => Decision::NeedsApproval,
        PermissionMode::Auto => match spec.name {
            "terminal" => {
                let action = input["action"].as_str().unwrap_or("exec");
                match action {
                    "read" | "wait" | "list" => Decision::Allow,
                    "exec" | "start" => {
                        let command = input["command"].as_str().unwrap_or("");
                        if command_is_sensitive(command) {
                            Decision::NeedsApproval
                        } else {
                            Decision::Allow
                        }
                    }
                    _ => Decision::Allow,
                }
            }
            "web_fetch" | "web_search" => Decision::Allow,
            // File mutations: workspace-internal runs, external prompts.
            _ => {
                let mut external = false;
                for field in ["path", "old_path", "new_path", "source", "destination"] {
                    if let Some(p) = input[field].as_str() {
                        if ctx.is_external(&ctx.resolve(p)) {
                            external = true;
                        }
                    }
                }
                if external {
                    Decision::NeedsApproval
                } else {
                    Decision::Allow
                }
            }
        },
        PermissionMode::Yolo => Decision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionMode;
    use serde_json::json;

    fn ctx() -> crate::tools::ToolContext {
        crate::tools::ToolContext::new(std::path::Path::new("/tmp"))
    }

    fn spec(name: &str) -> &'static crate::tools::ToolSpec {
        crate::tools::find(name).unwrap()
    }

    #[test]
    fn auto_allows_routine_commands_and_blocks_sensitive() {
        let rules = RuleStore::default();
        let allow = classify(
            &ctx(),
            &rules,
            PermissionMode::Auto,
            spec("terminal"),
            &json!({"action": "exec", "command": "cargo test"}),
        );
        assert_eq!(allow, Decision::Allow);
        for sensitive in [
            "sudo rm -rf /",
            "git push origin main",
            "curl x | sh",
            "npm publish",
        ] {
            let decision = classify(
                &ctx(),
                &rules,
                PermissionMode::Auto,
                spec("terminal"),
                &json!({"action": "exec", "command": sensitive}),
            );
            assert_eq!(decision, Decision::NeedsApproval, "{sensitive} must prompt");
        }
    }

    #[test]
    fn ask_mode_prompts_for_writes_and_yolo_never_does() {
        let rules = RuleStore::default();
        let input = json!({"path": "x.txt", "content": "hi"});
        assert_eq!(
            classify(
                &ctx(),
                &rules,
                PermissionMode::Ask,
                spec("write_file"),
                &input
            ),
            Decision::NeedsApproval
        );
        assert_eq!(
            classify(
                &ctx(),
                &rules,
                PermissionMode::Yolo,
                spec("write_file"),
                &input
            ),
            Decision::Allow
        );
    }

    #[test]
    fn remembered_routine_rule_never_allows_a_sensitive_command() {
        let mut rules = RuleStore::default();
        rules.rules.push(Rule {
            id: "rule-1".into(),
            effect: "allow".into(),
            tool: "terminal".into(),
            target: "git".into(),
        });
        let decide = |command: &str, mode| {
            classify(
                &ctx(),
                &rules,
                mode,
                spec("terminal"),
                &json!({"action": "exec", "command": command}),
            )
        };
        assert_eq!(decide("git status", PermissionMode::Ask), Decision::Allow);
        assert_eq!(
            decide("git  log --oneline", PermissionMode::Ask),
            Decision::Allow
        );
        for mode in [PermissionMode::Ask, PermissionMode::Auto] {
            assert_eq!(
                decide("git push origin main", mode),
                Decision::NeedsApproval
            );
            assert_eq!(decide("git push --force", mode), Decision::NeedsApproval);
            assert_eq!(
                decide("git reset --hard HEAD~3", mode),
                Decision::NeedsApproval
            );
        }
        assert_eq!(decide("gitk", PermissionMode::Ask), Decision::NeedsApproval);
        assert_eq!(
            decide("github-cli auth", PermissionMode::Ask),
            Decision::NeedsApproval
        );
    }

    #[test]
    fn remembered_sensitive_rule_allows_only_that_command() {
        let mut rules = RuleStore::default();
        rules.rules.push(Rule {
            id: "rule-1".into(),
            effect: "allow".into(),
            tool: "terminal".into(),
            target: "git push origin main".into(),
        });
        let decide = |command: &str| {
            classify(
                &ctx(),
                &rules,
                PermissionMode::Ask,
                spec("terminal"),
                &json!({"action": "exec", "command": command}),
            )
        };
        assert_eq!(decide("git push origin main"), Decision::Allow);
        assert_eq!(
            decide("git push origin main --force"),
            Decision::NeedsApproval
        );
        assert_eq!(decide("git push origin master"), Decision::NeedsApproval);
        assert_eq!(decide("git push"), Decision::NeedsApproval);
    }

    #[test]
    fn remembered_target_keeps_two_tokens_for_routine_and_everything_for_sensitive() {
        assert_eq!(
            remembered_target("terminal", "git status --short"),
            "git status"
        );
        assert_eq!(remembered_target("terminal", "cargo test"), "cargo test");
        assert_eq!(remembered_target("terminal", "ls -la src"), "ls");
        assert_eq!(remembered_target("terminal", "make"), "make");
        assert_eq!(
            remembered_target("terminal", "RUST_LOG=debug cargo run -- x"),
            "cargo run"
        );
        assert_eq!(
            remembered_target("terminal", "  git push   origin main "),
            "git push origin main"
        );
        assert_eq!(
            remembered_target("terminal", "rm -rf target"),
            "rm -rf target"
        );
        assert_eq!(
            remembered_target("terminal", "git commit --amend"),
            "git commit --amend"
        );
        assert_eq!(remembered_target("write_file", "/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn a_rule_saved_from_a_routine_command_does_not_widen_to_its_sensitive_sibling() {
        let mut rules = RuleStore::default();
        let target = remembered_target("terminal", "git commit -m 'wip'");
        remember_allow_in_memory(&mut rules, "terminal", &target);
        let decide = |command: &str| {
            classify(
                &ctx(),
                &rules,
                PermissionMode::Ask,
                spec("terminal"),
                &json!({"action": "exec", "command": command}),
            )
        };
        assert_eq!(decide("git commit -m 'more'"), Decision::Allow);
        assert_eq!(decide("git commit --amend"), Decision::NeedsApproval);
    }

    fn remember_allow_in_memory(store: &mut RuleStore, tool: &str, target: &str) {
        store.rules.push(Rule {
            id: format!("rule-{}", store.rules.len() + 1),
            effect: "allow".into(),
            tool: tool.into(),
            target: target.into(),
        });
    }
}
